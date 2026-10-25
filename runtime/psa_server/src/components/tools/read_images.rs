//! Image sniffing and CPU-only processing for `read`. The concrete tool owns
//! model-facing prompts and result formatting. Encoding uses native image-rs,
//! not Photon's WASM codec, so re-encoded bytes need not be identical to Pi.
use image::{
    DynamicImage, ImageEncoder,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
};
use std::{
    io::Cursor,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_DIMENSION: u32 = 2000;
const MAX_BASE64_BYTES: usize = 9 * 1024 * 1024 / 2;

pub(super) struct Processed {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
}

pub(super) fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return detect_png_mime(bytes);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        return detect_bmp_mime(bytes);
    }
    None
}

fn detect_png_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 16 || read_be_u32(bytes, 8) != 13 || bytes.get(12..16) != Some(b"IHDR") {
        return None;
    }

    let mut offset = 8usize;
    while offset + 8 <= bytes.len() {
        match &bytes[offset + 4..offset + 8] {
            b"acTL" => return None,
            b"IDAT" => break,
            _ => {}
        }

        let next = offset
            .checked_add(read_be_u32(bytes, offset) as usize)?
            .checked_add(12)?;
        if next <= offset || next > bytes.len() {
            break;
        }
        offset = next;
    }
    Some("image/png")
}

fn detect_bmp_mime(bytes: &[u8]) -> Option<&'static str> {
    let size = read_le_u32(bytes, 2);
    let pixel_offset = read_le_u32(bytes, 10);
    let dib_header_size = read_le_u32(bytes, 14);
    if (size != 0 && size < 26)
        || u64::from(pixel_offset) < 14 + u64::from(dib_header_size)
        || (size != 0 && pixel_offset >= size)
    {
        return None;
    }

    let (planes, depth) = if dib_header_size == 12 {
        (read_le_u16(bytes, 22), read_le_u16(bytes, 24))
    } else if (40..=124).contains(&dib_header_size) && bytes.len() >= 30 {
        (read_le_u16(bytes, 26), read_le_u16(bytes, 28))
    } else {
        return None;
    };
    (planes == 1 && [1, 4, 8, 16, 24, 32].contains(&depth)).then_some("image/bmp")
}

fn read_be_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .map_or(0, |value| u32::from_be_bytes(value.try_into().unwrap()))
}

fn read_le_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .map_or(0, |value| u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_le_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes
        .get(offset..offset + 2)
        .map_or(0, |value| u16::from_le_bytes(value.try_into().unwrap()))
}

fn find_exif_tiff_offset(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        return find_jpeg_exif_tiff_offset(bytes);
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return find_webp_exif_tiff_offset(bytes);
    }
    None
}

fn find_jpeg_exif_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 2usize;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }

        let marker = bytes[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        if offset + 4 > bytes.len() {
            return None;
        }
        if marker == 0xe1 && bytes.get(offset + 4..offset + 10) == Some(b"Exif\0\0") {
            return Some(offset + 10);
        }

        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset = offset.checked_add(2 + length)?;
    }
    None
}

fn find_webp_exif_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let size = read_le_u32(bytes, offset + 4) as usize;
        let start = offset + 8;
        let end = start.checked_add(size)?;
        if bytes.get(offset..offset + 4) != Some(b"EXIF") {
            offset = end.checked_add(size % 2)?;
            continue;
        }
        if end > bytes.len() {
            return None;
        }

        return Some(
            if size >= 6 && bytes.get(start..start + 6) == Some(b"Exif\0\0") {
                start + 6
            } else {
                start
            },
        );
    }
    None
}

fn parse_exif_orientation(bytes: &[u8]) -> u16 {
    let Some(start) = find_exif_tiff_offset(bytes) else {
        return 1;
    };
    if start + 8 > bytes.len() {
        return 1;
    }

    let little_endian = bytes.get(start..start + 2) == Some(b"II");
    let read16 = |offset| {
        if little_endian {
            read_le_u16(bytes, offset)
        } else {
            bytes
                .get(offset..offset + 2)
                .map_or(0, |value| u16::from_be_bytes(value.try_into().unwrap()))
        }
    };
    let relative = if little_endian {
        read_le_u32(bytes, start + 4)
    } else {
        read_be_u32(bytes, start + 4)
    };
    let Some(ifd) = start.checked_add(relative as usize) else {
        return 1;
    };
    if ifd.saturating_add(2) > bytes.len() {
        return 1;
    }

    for index in 0..usize::from(read16(ifd)) {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > bytes.len() {
            return 1;
        }
        if read16(entry) == 0x0112 {
            let value = read16(entry + 8);
            return if (1..=8).contains(&value) { value } else { 1 };
        }
    }
    1
}

fn apply_exif_orientation(image: DynamicImage, value: u16) -> DynamicImage {
    match value {
        2 => image.fliph(),
        3 => image.fliph().flipv(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let pixels = image.to_rgba8();
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            pixels.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(bytes)
}

fn fits_inline_size_limit(bytes: &[u8]) -> bool {
    bytes.len().div_ceil(3) * 4 < MAX_BASE64_BYTES
}

#[derive(Debug)]
pub(super) enum Failure {
    Conversion,
    Resize,
}

pub(super) fn prepare_image(
    mut bytes: Vec<u8>,
    mut mime: &'static str,
    cancelled: &AtomicBool,
) -> Result<Processed, Failure> {
    let failure = || {
        if mime == "image/bmp" {
            Failure::Conversion
        } else {
            Failure::Resize
        }
    };
    if cancelled.load(Ordering::Relaxed) {
        return Err(failure());
    }

    // Keep image-rs decoder allocation limits (512 MiB by default), in addition
    // to read_bytes' 64 MiB compressed source cap: a deliberate safety bound.
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| failure())?;
    let decoded = reader.decode().map_err(|_| failure())?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(failure());
    }

    let image = apply_exif_orientation(decoded, parse_exif_orientation(&bytes));
    if mime == "image/bmp" {
        bytes = encode_png(&image).ok_or(Failure::Conversion)?;
        mime = "image/png";
    }

    resize_image(image, bytes, mime, cancelled).ok_or(Failure::Resize)
}

fn resize_image(
    image: DynamicImage,
    bytes: Vec<u8>,
    mime: &'static str,
    cancelled: &AtomicBool,
) -> Option<Processed> {
    let original_width = image.width();
    let original_height = image.height();
    if original_width <= MAX_DIMENSION
        && original_height <= MAX_DIMENSION
        && fits_inline_size_limit(&bytes)
    {
        return Some(Processed {
            bytes,
            mime,
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            resized: false,
        });
    }

    let (mut width, mut height) = (original_width, original_height);
    if width > MAX_DIMENSION {
        height = (height as f64 * MAX_DIMENSION as f64 / width as f64).round() as u32;
        width = MAX_DIMENSION;
    }
    if height > MAX_DIMENSION {
        width = (width as f64 * MAX_DIMENSION as f64 / height as f64).round() as u32;
        height = MAX_DIMENSION;
    }
    // Pi can round an extremely thin image's other dimension to zero; its
    // resize fails in that case. Do not silently clamp it and change behavior.
    if width == 0 || height == 0 {
        return None;
    }

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }

        let resized = image.resize_exact(width, height, FilterType::Lanczos3);
        let candidate = encode_png(&resized)?;
        if fits_inline_size_limit(&candidate) {
            return Some(Processed {
                bytes: candidate,
                mime: "image/png",
                original_width,
                original_height,
                width,
                height,
                resized: true,
            });
        }

        for quality in [80, 85, 70, 55, 40] {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let mut candidate = Vec::new();
            JpegEncoder::new_with_quality(&mut candidate, quality)
                .encode_image(&DynamicImage::ImageRgb8(resized.to_rgb8()))
                .ok()?;
            if fits_inline_size_limit(&candidate) {
                return Some(Processed {
                    bytes: candidate,
                    mime: "image/jpeg",
                    original_width,
                    original_height,
                    width,
                    height,
                    resized: true,
                });
            }
        }

        if width == 1 && height == 1 {
            return None;
        }
        width = (width * 3 / 4).max(1);
        height = (height * 3 / 4).max(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;

    #[test]
    fn signatures_match_pi_including_apng_and_jpeg_ls_exclusions() {
        assert_eq!(detect_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(detect_mime(b"RIFF1234WEBP"), Some("image/webp"));
        assert_eq!(detect_mime(&[255, 216, 255]), Some("image/jpeg"));
        assert_eq!(detect_mime(&[255, 216, 255, 247]), None);
        assert_eq!(detect_mime(b"\x89PNG\r\n\x1a\n"), None);
        let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        assert_eq!(detect_mime(&header), Some("image/png"));
        header.resize(33, 0);
        header.extend_from_slice(b"\0\0\0\x08acTL");
        assert_eq!(detect_mime(&header), None);
        assert_eq!(detect_mime(b"BMthis is just text"), None);
    }

    #[test]
    fn original_bytes_are_preserved_and_big_images_are_resized() {
        let flag = AtomicBool::new(false);
        let image = DynamicImage::new_rgba8(10, 20);
        let original = encode_png(&image).unwrap();
        let result = prepare_image(original.clone(), "image/png", &flag).unwrap();
        assert_eq!(result.bytes, original);
        assert!(!result.resized);
        let image = DynamicImage::new_rgba8(2500, 1250);
        let result = prepare_image(encode_png(&image).unwrap(), "image/png", &flag).unwrap();
        assert!(result.resized);
        assert_eq!((result.width, result.height), (2000, 1000));
        assert_eq!(
            (result.original_width, result.original_height),
            (2500, 1250)
        );
        assert!(fits_inline_size_limit(&result.bytes));
        assert_eq!(
            image::guess_format(&result.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn bmp_conversion_and_cancelled_or_invalid_images() {
        let flag = AtomicBool::new(false);
        let mut buffer = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2, 3)
            .write_to(&mut buffer, ImageFormat::Bmp)
            .unwrap();
        let bytes = buffer.into_inner();
        assert_eq!(detect_mime(&bytes), Some("image/bmp"));
        let result = prepare_image(bytes, "image/bmp", &flag).unwrap();
        assert_eq!(result.mime, "image/png");
        assert!(!result.resized);
        assert!(prepare_image(b"not image".to_vec(), "image/png", &flag).is_err());
        flag.store(true, Ordering::Relaxed);
        assert!(prepare_image(result.bytes, "image/png", &flag).is_err());
    }

    #[test]
    fn exif_orientation_and_transpose() {
        let mut jpeg = vec![255, 216, 255, 225, 0, 34];
        jpeg.extend_from_slice(
            b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0",
        );
        assert_eq!(parse_exif_orientation(&jpeg), 6);
        let image = DynamicImage::new_rgb8(2, 3);
        let rotated = apply_exif_orientation(image, parse_exif_orientation(&jpeg));
        assert_eq!((rotated.width(), rotated.height()), (3, 2));
        assert_eq!(parse_exif_orientation(b"Exif\0\0"), 1);
    }
}
