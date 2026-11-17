use super::{DiscoveryError, normalize_absolute};
use crate::probes::ProbeHandle;
use psa::api::probes::{CHUNK_SIZE, Event};
use psa_shared::probe_transport::CallSender;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

struct Entry {
    relative: PathBuf,
    directory: bool,
    size: u64,
    mode: u32,
}

async fn entries(root: &Path) -> Result<Vec<Entry>, DiscoveryError> {
    if !tokio::fs::symlink_metadata(root).await?.is_dir() {
        return Err(DiscoveryError::Invalid(format!(
            "{} is not a skill directory",
            root.display()
        )));
    }
    let mut pending = vec![PathBuf::new()];
    let mut output = Vec::new();
    while let Some(relative) = pending.pop() {
        let mut directory = tokio::fs::read_dir(root.join(&relative)).await?;
        while let Some(entry) = directory.next_entry().await? {
            let relative = relative.join(entry.file_name());
            let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(DiscoveryError::Invalid(format!(
                    "skill contains unsupported entry {}",
                    entry.path().display()
                )));
            }
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = if metadata.is_dir() { 0o755 } else { 0o644 };
            if metadata.is_dir() {
                pending.push(relative.clone());
            }
            output.push(Entry {
                relative,
                directory: metadata.is_dir(),
                size: if metadata.is_dir() { 0 } else { metadata.len() },
                mode,
            });
        }
    }
    output.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(output)
}

/// Hash names, types, modes and every file byte, not just SKILL.md or timestamps.
async fn hash_directory(root: &Path) -> Result<String, DiscoveryError> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK_SIZE];
    for entry in entries(root).await? {
        hash.update(headers(&entry)?);
        if !entry.directory {
            let mut file = tokio::fs::File::open(root.join(&entry.relative)).await?;
            loop {
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn headers(entry: &Entry) -> Result<Vec<u8>, DiscoveryError> {
    let mut output = Vec::new();
    let mut header = tar::Header::new_gnu();
    if header.set_path(&entry.relative).is_err() {
        // GNU long-name records preserve arbitrary supported source paths while
        // keeping archive construction bounded to one header/path at a time.
        let path = entry.relative.as_os_str().as_encoded_bytes();
        let mut long = tar::Header::new_gnu();
        long.set_path("././@LongLink")?;
        long.set_entry_type(tar::EntryType::GNULongName);
        long.set_size(path.len() as u64 + 1);
        long.set_mode(0o644);
        long.set_cksum();
        output.extend_from_slice(long.as_bytes());
        output.extend_from_slice(path);
        output.push(0);
        output.resize(output.len().next_multiple_of(512), 0);
        header.set_path("long-name")?;
    }
    header.set_entry_type(if entry.directory {
        tar::EntryType::Directory
    } else {
        tar::EntryType::Regular
    });
    header.set_size(entry.size);
    header.set_mode(entry.mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    output.extend_from_slice(header.as_bytes());
    Ok(output)
}

async fn send_archive(
    sender: CallSender,
    source: &Path,
    expected: &str,
) -> Result<(), DiscoveryError> {
    let mut buffer = vec![0; CHUNK_SIZE];
    let mut hash = Sha256::new();
    for entry in entries(source).await? {
        let headers = headers(&entry)?;
        hash.update(&headers);
        sender.write(0, &headers).await?;
        if !entry.directory {
            let mut file = tokio::fs::File::open(source.join(&entry.relative)).await?;
            let mut remaining = entry.size;
            while remaining > 0 {
                let limit = buffer.len().min(remaining as usize);
                let count = file.read(&mut buffer[..limit]).await?;
                if count == 0 {
                    return Err(DiscoveryError::Invalid(
                        "skill changed during transfer".into(),
                    ));
                }
                hash.update(&buffer[..count]);
                sender.write(0, &buffer[..count]).await?;
                remaining -= count as u64;
            }
            if file.read(&mut buffer[..1]).await? != 0 {
                return Err(DiscoveryError::Invalid(
                    "skill changed during transfer".into(),
                ));
            }
            let padding = (512 - entry.size % 512) % 512;
            sender.write(0, &vec![0; padding as usize]).await?;
        }
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(DiscoveryError::Invalid(
            "skill changed during transfer; retry creation".into(),
        ));
    }
    sender.write(0, &[0; 1024]).await?;
    sender.close_stdin().await?;
    Ok(())
}

/// Stable managed destinations are replaced through SyncDir. A sibling digest
/// records successful publication; existence is checked even after reconnects.
pub(super) async fn sync_directory(
    probe: &ProbeHandle,
    source: &Path,
    destination: &Path,
) -> Result<PathBuf, DiscoveryError> {
    let destination = normalize_absolute(destination)?;
    let hash = hash_directory(source).await?;
    let parent = destination
        .parent()
        .ok_or_else(|| DiscoveryError::Invalid("cache destination has no parent".into()))?;
    probe.create_dir(parent).await?;
    let marker = parent.join(format!(
        "{}.sha256",
        destination.file_name().unwrap().to_string_lossy()
    ));
    let listing = probe.list_dir(parent).await?;
    if let Some(existing) = listing
        .iter()
        .find(|entry| entry.name == destination.file_name().unwrap().to_string_lossy())
    {
        if existing.kind != "directory" {
            return Err(DiscoveryError::Invalid(
                "managed skill cache is not a directory".into(),
            ));
        }
        if let Some(existing) = listing
            .iter()
            .find(|entry| entry.name == marker.file_name().unwrap().to_string_lossy())
        {
            if existing.kind != "file" {
                return Err(DiscoveryError::Invalid(
                    "managed skill digest is not a file".into(),
                ));
            }
            if super::Source::Probe(probe).read(&marker).await? == hash.as_bytes() {
                return Ok(destination);
            }
        }
    }
    let mut call = probe.sync_dir(&destination).await?;
    let sender = call.sender.clone();
    tokio::try_join!(send_archive(sender, source, &hash), async {
        loop {
            match call.next_event().await? {
                Event::Complete { status: None } => return Ok::<_, DiscoveryError>(()),
                Event::Started { .. } => {}
                _ => {
                    return Err(DiscoveryError::Invalid(
                        "unexpected skill sync response".into(),
                    ));
                }
            }
        }
    })?;
    let mut marker_call = probe.write_file(marker, false).await?;
    let sender = marker_call.sender.clone();
    tokio::try_join!(
        async {
            sender.write(0, hash.as_bytes()).await?;
            sender.close_stdin().await?;
            Ok::<_, DiscoveryError>(())
        },
        async {
            match marker_call.next_event().await? {
                Event::Complete { status: None } => Ok::<_, DiscoveryError>(()),
                _ => Err(DiscoveryError::Invalid(
                    "unexpected digest write response".into(),
                )),
            }
        }
    )?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_supporting_file_contents_and_removals() {
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(directory.path().join("SKILL.md"), "metadata")
            .await
            .unwrap();
        let first = hash_directory(directory.path()).await.unwrap();
        tokio::fs::write(directory.path().join("script.py"), "first")
            .await
            .unwrap();
        let second = hash_directory(directory.path()).await.unwrap();
        tokio::fs::write(directory.path().join("script.py"), "other")
            .await
            .unwrap();
        let third = hash_directory(directory.path()).await.unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        tokio::fs::remove_file(directory.path().join("script.py"))
            .await
            .unwrap();
        assert_eq!(first, hash_directory(directory.path()).await.unwrap());
    }
}
