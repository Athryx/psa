//! Directory trees sent over the wire as tar archives.
//!
//! An [`Archive`] holds the contents of one directory: every tar entry path is
//! relative to that directory. Only regular files and directories are used.
//! Skill uploads and sandbox mount seeds share this format.
use serde::{Deserialize, Serialize};
use std::{
    fs::Metadata,
    io,
    path::{Component, Path, PathBuf},
};

/// Contents of a directory as a tar archive, optionally zstd-compressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    /// Tar archive, compressed as `compression` says. Base64-encoded on the wire.
    #[serde(with = "base64_bytes")]
    pub tar_data: Vec<u8>,
    /// Compression applied to `tar_data`; defaults to none.
    #[serde(default)]
    pub compression: ArchiveCompression,
}

/// Compression of an archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveCompression {
    /// A plain tar archive.
    #[default]
    None,
    /// A zstd-compressed tar archive.
    Zstd,
}

/// Zstd level for archives: zstd's default, fast with a good ratio.
const ZSTD_LEVEL: i32 = 3;

impl Archive {
    /// Reproducible archive of the directory at `path`. Symlinks are followed;
    /// symlink cycles and special files are rejected.
    pub async fn from_directory(path: impl AsRef<Path>) -> io::Result<Self> {
        let root = tokio::fs::canonicalize(path).await?;
        let tar = build_directory_tar(&root).await.map_err(io::Error::other)?;
        tokio::task::spawn_blocking(move || Self::compress_tar(&tar))
            .await
            .map_err(io::Error::other)?
    }

    /// Archive for files built in memory.
    pub fn builder() -> ArchiveBuilder {
        ArchiveBuilder::default()
    }

    /// Zstd-compress a plain tar archive.
    pub fn compress_tar(tar: &[u8]) -> io::Result<Self> {
        Ok(Self {
            tar_data: zstd::bulk::compress(tar, ZSTD_LEVEL)?,
            compression: ArchiveCompression::Zstd,
        })
    }
}

/// Builds an [`Archive`] from in-memory files and directories. Parent
/// directories of added paths need not be added themselves.
#[derive(Debug, Clone, Default)]
pub struct ArchiveBuilder {
    entries: Vec<TarEntry>,
}

impl ArchiveBuilder {
    /// Add a regular file with mode `0644`.
    pub fn file(self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        self.entry(path.into(), 0o644, Some(contents.into()))
    }

    /// Add an executable file with mode `0755`.
    pub fn executable(self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        self.entry(path.into(), 0o755, Some(contents.into()))
    }

    /// Add a directory with mode `0755`.
    pub fn dir(self, path: impl Into<PathBuf>) -> Self {
        self.entry(path.into(), 0o755, None)
    }

    /// Build the compressed archive. Paths must be relative, without `..`,
    /// and added at most once.
    pub fn build(mut self) -> io::Result<Archive> {
        for entry in &self.entries {
            if !is_plain_relative_path(&entry.path) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "archive path {} must be relative without '..'",
                        entry.path.display()
                    ),
                ));
            }
        }
        self.entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(pair) = self
            .entries
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive path {} added twice", pair[0].path.display()),
            ));
        }

        let tar = write_tar(self.entries).map_err(io::Error::other)?;
        Archive::compress_tar(&tar)
    }

    fn entry(mut self, path: PathBuf, mode: u32, contents: Option<Vec<u8>>) -> Self {
        self.entries.push(TarEntry {
            path,
            mode,
            contents,
        });
        self
    }
}

/// Whether `path` is nonempty, relative, and made only of normal components.
fn is_plain_relative_path(path: &Path) -> bool {
    path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// A file or directory in a tar archive; directories have no contents.
#[derive(Debug, Clone)]
struct TarEntry {
    path: PathBuf,
    mode: u32,
    contents: Option<Vec<u8>>,
}

/// Build a reproducible plain tar archive of `root`: entries are sorted and
/// carry no timestamps or owners, so unchanged content hashes the same.
pub(crate) async fn build_directory_tar(root: &Path) -> Result<Vec<u8>, String> {
    let entries = collect_entries(root).await?;
    tokio::task::spawn_blocking(move || write_tar(entries))
        .await
        .map_err(|error| error.to_string())?
}

/// Write `entries` in order as a tar archive without timestamps or owners.
fn write_tar(entries: Vec<TarEntry>) -> Result<Vec<u8>, String> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(entry.mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        let contents = entry.contents.unwrap_or_else(|| {
            header.set_entry_type(tar::EntryType::Directory);
            Vec::new()
        });
        header.set_size(contents.len() as u64);
        builder
            .append_data(&mut header, &entry.path, contents.as_slice())
            .map_err(|error| format!("{}: {error}", entry.path.display()))?;
    }
    builder.into_inner().map_err(|error| error.to_string())
}

/// Every file and directory under `root`, following symlinks, sorted by path.
async fn collect_entries(root: &Path) -> Result<Vec<TarEntry>, String> {
    let io_error = |path: &Path, error: io::Error| format!("{}: {error}", path.display());

    let mut entries = Vec::new();
    // Each pending directory carries the canonical directories above it, so a
    // symlink back to one of them is reported instead of walked forever.
    let mut pending = vec![(root.to_owned(), PathBuf::new(), vec![root.to_owned()])];
    while let Some((directory, relative, ancestors)) = pending.pop() {
        let mut children = tokio::fs::read_dir(&directory)
            .await
            .map_err(|error| io_error(&relative, error))?;
        while let Some(child) = children
            .next_entry()
            .await
            .map_err(|error| io_error(&relative, error))?
        {
            let source = child.path();
            let path = relative.join(child.file_name());
            let metadata = tokio::fs::metadata(&source)
                .await
                .map_err(|error| io_error(&path, error))?;

            if metadata.is_file() {
                let contents = tokio::fs::read(&source)
                    .await
                    .map_err(|error| io_error(&path, error))?;
                entries.push(TarEntry {
                    path,
                    mode: entry_mode(&metadata, 0o644),
                    contents: Some(contents),
                });
                continue;
            }
            if !metadata.is_dir() {
                return Err(format!("{}: not a file or directory", path.display()));
            }

            let canonical = tokio::fs::canonicalize(&source)
                .await
                .map_err(|error| io_error(&path, error))?;
            if ancestors.contains(&canonical) {
                return Err(format!("{}: symlink cycle", path.display()));
            }
            entries.push(TarEntry {
                path: path.clone(),
                mode: entry_mode(&metadata, 0o755),
                contents: None,
            });
            let mut ancestors = ancestors.clone();
            ancestors.push(canonical.clone());
            pending.push((canonical, path, ancestors));
        }
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

#[cfg(unix)]
fn entry_mode(metadata: &Metadata, _default: u32) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn entry_mode(_metadata: &Metadata, default: u32) -> u32 {
    default
}

/// Serde adapter encoding bytes as a standard base64 string.
mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_entries(archive: &Archive) -> Vec<(String, u32, Option<Vec<u8>>)> {
        assert_eq!(archive.compression, ArchiveCompression::Zstd);
        let tar = zstd::bulk::decompress(&archive.tar_data, 1 << 20).unwrap();
        let mut reader = tar::Archive::new(tar.as_slice());
        reader
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mode = entry.header().mode().unwrap();
                if entry.header().entry_type().is_dir() {
                    return (path, mode, None);
                }
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                (path, mode, Some(contents))
            })
            .collect()
    }

    #[test]
    fn archives_encode_as_base64_and_default_to_uncompressed() {
        let archive = Archive {
            tar_data: vec![0, 1, 255],
            compression: ArchiveCompression::Zstd,
        };

        let value = serde_json::to_value(&archive).unwrap();
        assert_eq!(value["tar_data"], "AAH/");
        assert_eq!(value["compression"], "Zstd");
        assert_eq!(serde_json::from_value::<Archive>(value).unwrap(), archive);

        let plain: Archive = serde_json::from_str(r#"{"tar_data":"AAH/"}"#).unwrap();
        assert_eq!(plain.compression, ArchiveCompression::None);
        assert!(serde_json::from_str::<Archive>(r#"{"tar_data":"","x":1}"#).is_err());
    }

    #[test]
    fn builder_sorts_entries_and_sets_modes() {
        let archive = Archive::builder()
            .executable("run.sh", "#!/bin/sh")
            .dir("empty")
            .file("src/lib.rs", "fn main() {}")
            .build()
            .unwrap();

        assert_eq!(
            read_entries(&archive),
            [
                ("empty".into(), 0o755, None),
                ("run.sh".into(), 0o755, Some(b"#!/bin/sh".to_vec())),
                ("src/lib.rs".into(), 0o644, Some(b"fn main() {}".to_vec())),
            ]
        );
    }

    #[test]
    fn builder_rejects_unsafe_and_duplicate_paths() {
        for path in ["/abs", "../up", "a/../b", "./a", ""] {
            assert!(
                Archive::builder().file(path, "x").build().is_err(),
                "{path}"
            );
        }
        assert!(Archive::builder().file("a", "x").dir("a").build().is_err());
    }
}
