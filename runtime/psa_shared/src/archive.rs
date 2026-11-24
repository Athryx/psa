//! Reading and extracting [`Archive`]s received over the wire.
//!
//! Archives hold only regular files and directories at relative paths without
//! `..`. Extraction never follows symlinks under the destination, never
//! replaces existing files, and merges into existing directories.
use psa::api::{Archive, ArchiveCompression};
use rustix::{
    fs::{AtFlags, Mode, OFlags},
    io::Errno,
};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    io::{self, Cursor, Write},
    ops::Range,
    os::fd::OwnedFd,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// Bounds on an archive's size, checked while reading it.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest uncompressed tar size.
    pub max_unpacked_bytes: usize,
    /// Most entries, files and directories together.
    pub max_entries: usize,
}

/// An archive that cannot be read or extracted.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The archive content is unacceptable, or conflicts with the destination.
    #[error("{0}")]
    Invalid(String),
    /// The archive is larger than its limits allow.
    #[error("archive exceeds {0} bytes uncompressed")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<Errno> for ArchiveError {
    fn from(errno: Errno) -> Self {
        Self::Io(errno.into())
    }
}

/// One validated archive entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Normalized relative path, made only of normal components.
    pub path: PathBuf,
    /// Whether the entry is a directory rather than a regular file.
    pub directory: bool,
    /// Permission bits.
    pub mode: u32,
    contents: Range<usize>,
}

/// The validated entries of a decompressed archive. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Entries {
    tar: Arc<[u8]>,
    entries: Arc<[Entry]>,
}

impl Entries {
    /// The uncompressed tar archive.
    pub fn tar(&self) -> &[u8] {
        &self.tar
    }

    /// Entries in archive order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Contents of a file entry; empty for directories.
    pub fn contents(&self, entry: &Entry) -> &[u8] {
        &self.tar[entry.contents.clone()]
    }
}

/// Decompress `archive` within `limits` and validate every entry: only files
/// and directories, relative paths without `..`, and no duplicates.
pub async fn read_entries(archive: &Archive, limits: Limits) -> Result<Entries, ArchiveError> {
    let too_large = || ArchiveError::TooLarge(limits.max_unpacked_bytes);
    if archive.tar_data.len() > limits.max_unpacked_bytes {
        return Err(too_large());
    }

    let tar: Arc<[u8]> = match archive.compression {
        ArchiveCompression::None => archive.tar_data.as_slice().into(),
        ArchiveCompression::Zstd => {
            let compressed = archive.tar_data.clone();
            tokio::task::spawn_blocking(move || {
                zstd::bulk::decompress(&compressed, limits.max_unpacked_bytes)
            })
            .await
            .map_err(io::Error::other)?
            .map_err(|_| too_large())?
            .into()
        }
    };

    let entries = parse_entries(&tar, limits)?;
    Ok(Entries {
        tar,
        entries: entries.into(),
    })
}

fn parse_entries(tar: &[u8], limits: Limits) -> Result<Vec<Entry>, ArchiveError> {
    let invalid = |reason: &str| ArchiveError::Invalid(reason.to_owned());

    let mut archive = tar::Archive::new(Cursor::new(tar));
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(invalid("archive path escapes its directory"));
        }
        let path: PathBuf = path
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .collect();

        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(invalid("archives only support files and directories"));
        }
        if path.as_os_str().is_empty() {
            // The root directory itself.
            if kind.is_dir() {
                continue;
            }
            return Err(invalid("empty archive file name"));
        }
        if !seen.insert(path.clone()) {
            return Err(ArchiveError::Invalid(format!(
                "duplicate archive path {}",
                path.display()
            )));
        }

        let start = usize::try_from(entry.raw_file_position())
            .map_err(|_| invalid("archive offset overflow"))?;
        let size = usize::try_from(entry.size()).map_err(|_| invalid("archive size overflow"))?;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= tar.len())
            .ok_or_else(|| invalid("truncated archive"))?;
        result.push(Entry {
            path,
            directory: kind.is_dir(),
            mode: entry.header().mode()? & 0o777,
            contents: if kind.is_dir() { 0..0 } else { start..end },
        });
        if result.len() > limits.max_entries {
            return Err(ArchiveError::Invalid(format!(
                "archive has more than {} entries",
                limits.max_entries
            )));
        }
    }
    Ok(result)
}

/// Files and directories an extraction created, so they can be removed again.
#[derive(Debug)]
pub struct Extracted {
    root: PathBuf,
    /// Relative paths in creation order, with whether each is a directory.
    created: Vec<(PathBuf, bool)>,
}

impl Extracted {
    /// Remove everything the extraction created, best effort. Directories that
    /// already existed are kept, as are files added to them since.
    pub async fn remove(self) {
        let _ =
            tokio::task::spawn_blocking(move || remove_created(&self.root, &self.created)).await;
    }
}

/// Extract `entries` under the existing directory `root`, which may be reached
/// through a symlink. Existing directories are merged; a file entry whose path
/// exists, or a path through a symlink or non-directory below `root`, is an
/// error. On error everything created is removed again.
pub async fn extract_into(entries: &Entries, root: &Path) -> Result<Extracted, ArchiveError> {
    let entries = entries.clone();
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut created = Vec::new();
        match extract_blocking(&entries, &root, &mut created) {
            Ok(()) => Ok(Extracted { root, created }),
            Err(error) => {
                remove_created(&root, &created);
                Err(error)
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

/// Flags for the destination root, which may itself be a symlink.
const ROOT_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC);

/// Flags for directories under the root, which must not be symlinks.
const DIRECTORY_FLAGS: OFlags = ROOT_FLAGS.union(OFlags::NOFOLLOW);

/// Mode for directories created only as parents of archive entries.
const PARENT_MODE: u32 = 0o755;

fn extract_blocking(
    entries: &Entries,
    root: &Path,
    created: &mut Vec<(PathBuf, bool)>,
) -> Result<(), ArchiveError> {
    let root_fd = rustix::fs::open(root, ROOT_FLAGS, Mode::empty())?;
    // Final modes of the directories this extraction created. They stay
    // writable until every entry is in place.
    let mut directory_modes = HashMap::new();

    for entry in entries.entries() {
        let parent_path = entry.path.parent().unwrap_or(Path::new(""));
        let parent = ensure_directories(&root_fd, parent_path, created, &mut directory_modes)?;
        let name = entry.path.file_name().expect("entry paths are nonempty");

        if entry.directory {
            let (_, newly_created) = ensure_directory(&parent, name, &entry.path)?;
            if newly_created {
                created.push((entry.path.clone(), true));
            }
            if let Some(mode) = directory_modes.get_mut(&entry.path) {
                *mode = entry.mode;
            } else if newly_created {
                directory_modes.insert(entry.path.clone(), entry.mode);
            }
            continue;
        }

        let file = rustix::fs::openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|errno| match errno {
            Errno::EXIST => {
                ArchiveError::Invalid(format!("{} already exists", entry.path.display()))
            }
            errno => errno.into(),
        })?;
        created.push((entry.path.clone(), false));
        let mut file = std::fs::File::from(file);
        file.write_all(entries.contents(entry))?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(entry.mode))?;
    }

    // Deepest first, so read-only directories do not block their children.
    let mut directory_modes: Vec<_> = directory_modes.into_iter().collect();
    directory_modes.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directory_modes {
        let directory = open_directory(&root_fd, &path)?;
        rustix::fs::fchmod(&directory, Mode::from_raw_mode(mode))?;
    }
    Ok(())
}

/// Open `relative` under `root` as a directory, creating missing components.
fn ensure_directories(
    root: &OwnedFd,
    relative: &Path,
    created: &mut Vec<(PathBuf, bool)>,
    directory_modes: &mut HashMap<PathBuf, u32>,
) -> Result<OwnedFd, ArchiveError> {
    let mut current = rustix::io::dup(root)?;
    let mut path = PathBuf::new();
    for component in relative.components() {
        path.push(component);
        let (directory, newly_created) = ensure_directory(&current, component.as_os_str(), &path)?;
        if newly_created {
            created.push((path.clone(), true));
            directory_modes.insert(path.clone(), PARENT_MODE);
        }
        current = directory;
    }
    Ok(current)
}

/// Open the directory `name` in `parent` without following symlinks, creating
/// it when missing. Returns it and whether it was created.
fn ensure_directory(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
) -> Result<(OwnedFd, bool), ArchiveError> {
    let created = match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(errno) => return Err(errno.into()),
    };
    match rustix::fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(directory) => Ok((directory, created)),
        Err(Errno::NOTDIR | Errno::LOOP) => Err(ArchiveError::Invalid(format!(
            "{} exists and is not a directory",
            path.display()
        ))),
        Err(errno) => Err(errno.into()),
    }
}

/// Open the existing directory `relative` under `root` without following symlinks.
fn open_directory(root: &OwnedFd, relative: &Path) -> Result<OwnedFd, Errno> {
    let mut current = rustix::io::dup(root)?;
    for component in relative.components() {
        current = rustix::fs::openat(
            &current,
            component.as_os_str(),
            DIRECTORY_FLAGS,
            Mode::empty(),
        )?;
    }
    Ok(current)
}

/// Remove `created` (relative to `root`) in reverse creation order, ignoring failures.
fn remove_created(root: &Path, created: &[(PathBuf, bool)]) {
    let Ok(root_fd) = rustix::fs::open(root, ROOT_FLAGS, Mode::empty()) else {
        return;
    };

    // Make created directories writable again, shallowest first so each stays reachable.
    let mut directories: Vec<_> = created
        .iter()
        .filter(|(_, directory)| *directory)
        .map(|(path, _)| path)
        .collect();
    directories.sort_by_key(|path| path.components().count());
    for path in directories {
        if let Ok(directory) = open_directory(&root_fd, path) {
            let _ = rustix::fs::fchmod(&directory, Mode::from_raw_mode(0o700));
        }
    }

    for (path, directory) in created.iter().rev() {
        let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
            continue;
        };
        let Ok(parent) = open_directory(&root_fd, parent) else {
            continue;
        };
        let flags = if *directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        let _ = rustix::fs::unlinkat(&parent, name, flags);
    }
}

#[cfg(test)]
mod tests;
