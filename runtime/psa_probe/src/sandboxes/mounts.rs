//! Host-side preparation of sandbox mounts before the container is created.
use super::{Result, invalid};
use psa::api::{HostPolicy, MAX_SANDBOX_SEED_UNPACKED_BYTES, Mount};
use psa_shared::archive::{self, Extracted};
use std::{
    io,
    path::{Path, PathBuf},
};

const SEED_LIMITS: archive::Limits = archive::Limits {
    max_unpacked_bytes: MAX_SANDBOX_SEED_UNPACKED_BYTES,
    max_entries: 10_000,
};

/// Host changes made while preparing mounts, removed again if creation fails.
#[derive(Debug, Default)]
pub struct PreparedMounts {
    /// Topmost directory created for each mount whose host path was missing.
    created_directories: Vec<PathBuf>,
    seeded: Vec<Extracted>,
}

impl PreparedMounts {
    /// Remove seeded files and created directories, latest first, best effort.
    pub async fn remove(self) {
        for extracted in self.seeded.into_iter().rev() {
            extracted.remove().await;
        }
        for directory in self.created_directories.into_iter().rev() {
            let _ = tokio::fs::remove_dir_all(directory).await;
        }
    }
}

/// Check or create each mount's host directory and extract its seed. Changes
/// are recorded in `prepared` as they happen, so a failure part way through
/// can be undone.
pub async fn prepare_mounts(mounts: &[Mount], prepared: &mut PreparedMounts) -> Result<()> {
    for mount in mounts {
        prepare_mount(mount, prepared)
            .await
            .map_err(|error| format!("mount {}: {error}", mount.host_path.display()))?;
    }
    Ok(())
}

async fn prepare_mount(mount: &Mount, prepared: &mut PreparedMounts) -> Result<()> {
    if let Some(created) = ensure_host_directory(&mount.host_path, mount.host).await? {
        prepared.created_directories.push(created);
    }

    let Some(seed) = &mount.seed else {
        return Ok(());
    };
    let entries = archive::read_entries(seed, SEED_LIMITS).await?;
    let extracted = archive::extract_into(&entries, &mount.host_path).await?;
    prepared.seeded.push(extracted);
    Ok(())
}

/// Make sure `path` is a directory as `policy` requires. Returns the topmost
/// directory created, if the path was missing and got created.
async fn ensure_host_directory(path: &Path, policy: HostPolicy) -> Result<Option<PathBuf>> {
    let existing = match tokio::fs::metadata(path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    match (policy, existing) {
        (_, Some(metadata)) if !metadata.is_dir() => {
            Err(invalid("host path exists and is not a directory"))
        }
        (HostPolicy::Existing | HostPolicy::CreateOrReuse, Some(_)) => Ok(None),
        (HostPolicy::Existing, None) => Err(invalid("host directory does not exist")),
        (HostPolicy::Create, Some(_)) => Err(invalid("host directory already exists")),
        (HostPolicy::Create | HostPolicy::CreateOrReuse, None) => {
            if tokio::fs::symlink_metadata(path).await.is_ok() {
                return Err(invalid("host path is a dangling symlink"));
            }
            create_directory(path).await.map(Some)
        }
    }
}

/// Create `path` with its missing parents, returning the topmost one created.
async fn create_directory(path: &Path) -> Result<PathBuf> {
    let mut topmost = path;
    while let Some(parent) = topmost.parent() {
        if tokio::fs::symlink_metadata(parent).await.is_ok() {
            break;
        }
        topmost = parent;
    }
    tokio::fs::create_dir_all(path).await?;
    Ok(topmost.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa::api::Archive;

    fn mount(host_path: &Path, host: HostPolicy, seed: Option<Archive>) -> Mount {
        Mount {
            host_path: host_path.to_owned(),
            container_path: "/work".into(),
            writable: true,
            host,
            seed,
        }
    }

    async fn prepare(mounts: &[Mount]) -> (Result<()>, PreparedMounts) {
        let mut prepared = PreparedMounts::default();
        let result = prepare_mounts(mounts, &mut prepared).await;
        (result, prepared)
    }

    #[tokio::test]
    async fn policies_check_or_create_host_directories() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let missing = temp.path().join("a/b/c");

        for (path, policy, ok) in [
            (&existing, HostPolicy::Existing, true),
            (&existing, HostPolicy::CreateOrReuse, true),
            (&existing, HostPolicy::Create, false),
            (&missing, HostPolicy::Existing, false),
            (&file, HostPolicy::Existing, false),
            (&file, HostPolicy::CreateOrReuse, false),
        ] {
            let (result, _) = prepare(&[mount(path, policy, None)]).await;
            assert_eq!(result.is_ok(), ok, "{} {policy:?}", path.display());
        }

        let (result, prepared) = prepare(&[mount(&missing, HostPolicy::Create, None)]).await;
        result.unwrap();
        assert!(missing.is_dir());
        prepared.remove().await;
        assert!(!temp.path().join("a").exists());
    }

    #[tokio::test]
    async fn seeds_are_extracted_and_undone_when_a_later_mount_fails() {
        let temp = tempfile::tempdir().unwrap();
        let reused = temp.path().join("reused");
        std::fs::create_dir_all(reused.join("src")).unwrap();
        std::fs::write(reused.join("src/old.rs"), "old").unwrap();
        let created = temp.path().join("new/work");
        let seed = Archive::builder()
            .file("TASK.md", "do it")
            .file("src/new.rs", "new")
            .build()
            .unwrap();

        let mounts = [
            mount(&created, HostPolicy::Create, Some(seed.clone())),
            mount(&reused, HostPolicy::CreateOrReuse, Some(seed.clone())),
        ];
        let (result, prepared) = prepare(&mounts).await;
        result.unwrap();
        assert_eq!(std::fs::read(created.join("TASK.md")).unwrap(), b"do it");
        assert_eq!(std::fs::read(reused.join("src/new.rs")).unwrap(), b"new");
        prepared.remove().await;
        assert!(!temp.path().join("new").exists());
        assert!(!reused.join("TASK.md").exists());
        assert!(reused.join("src/old.rs").exists());

        // The second mount conflicts with the seed already extracted into it.
        let mounts = [
            mount(&created, HostPolicy::Create, Some(seed.clone())),
            mount(&created, HostPolicy::CreateOrReuse, Some(seed)),
        ];
        let (result, prepared) = prepare(&mounts).await;
        assert!(result.unwrap_err().to_string().contains("already exists"));
        prepared.remove().await;
        assert!(!temp.path().join("new").exists());
    }
}
