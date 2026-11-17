use super::{ConfigRegistry, DiscoveryError, skills::parse_skill};
use crate::db::SkillRegistration;
use psa_api::{NewSkill, SkillLocation};
use sha2::{Digest, Sha256};
use std::{
    io::Cursor,
    path::{Component, Path, PathBuf},
};

struct UploadStaging(Option<PathBuf>);

impl Drop for UploadStaging {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    cleanup_upload(&path).await;
                });
            }
        }
    }
}

struct UploadEntry {
    path: PathBuf,
    directory: bool,
    start: usize,
    size: usize,
    mode: u32,
}

fn validate_archive(data: &[u8]) -> Result<Vec<UploadEntry>, DiscoveryError> {
    if data.len() > 64 * 1024 * 1024 {
        return Err(DiscoveryError::Invalid(
            "skill upload exceeds 64 MiB".into(),
        ));
    }
    let mut archive = tar::Archive::new(Cursor::new(data));
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(DiscoveryError::Invalid("unsafe skill archive path".into()));
        }
        let path: PathBuf = path
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .collect();
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(DiscoveryError::Invalid(
                "skill archive only supports files and directories".into(),
            ));
        }
        if path.as_os_str().is_empty() {
            if kind.is_dir() {
                continue;
            }
            return Err(DiscoveryError::Invalid(
                "empty skill archive filename".into(),
            ));
        }
        if !seen.insert(path.clone()) {
            return Err(DiscoveryError::Invalid(
                "duplicate skill archive path".into(),
            ));
        }
        let start = usize::try_from(entry.raw_file_position())
            .map_err(|_| DiscoveryError::Invalid("archive offset overflow".into()))?;
        let size = usize::try_from(entry.size())
            .map_err(|_| DiscoveryError::Invalid("archive size overflow".into()))?;
        if start.checked_add(size).is_none_or(|end| end > data.len()) {
            return Err(DiscoveryError::Invalid("truncated skill archive".into()));
        }
        result.push(UploadEntry {
            path,
            directory: kind.is_dir(),
            start,
            size,
            mode: entry.header().mode()? & 0o777,
        });
        if result.len() > 10_000 {
            return Err(DiscoveryError::Invalid(
                "too many skill archive entries".into(),
            ));
        }
    }
    Ok(result)
}

impl ConfigRegistry {
    /// Remove only a managed API upload, never global/project source directories.
    /// Call after replacing its DB registration, or rolling back a failed upsert.
    pub async fn remove_uploaded_source(
        &self,
        location: &SkillLocation,
    ) -> Result<(), DiscoveryError> {
        let _guard = self.gate.lock().await;
        let SkillLocation::Global(path) = location else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let managed = self.data_dir.join("skill-uploads");
        let managed = match tokio::fs::canonicalize(managed).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if tokio::fs::canonicalize(parent).await? != managed {
            return Ok(());
        }
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.is_dir() => {
                let canonical = tokio::fs::canonicalize(path).await?;
                if canonical.parent() != Some(managed.as_path()) {
                    return Err(DiscoveryError::Invalid(
                        "upload cleanup target is outside managed storage".into(),
                    ));
                }
                cleanup_upload(&canonical).await;
            }
            Ok(_) => {
                return Err(DiscoveryError::Invalid(
                    "managed upload is not a directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        if tokio::fs::try_exists(path).await? {
            return Err(DiscoveryError::Invalid(
                "could not remove managed upload".into(),
            ));
        }
        Ok(())
    }

    /// Accept uploaded content, never a client-asserted server path. Metadata uses
    /// the same parser as global and probe discovery. Database upsert is separate.
    pub async fn upload_skill(
        &self,
        upload: &NewSkill,
    ) -> Result<SkillRegistration, DiscoveryError> {
        let _guard = self.gate.lock().await;
        let entries = validate_archive(&upload.tar_data)?;
        let metadata = entries
            .iter()
            .find(|entry| entry.path == Path::new("SKILL.md") && !entry.directory)
            .ok_or_else(|| {
                DiscoveryError::Invalid("skill archive must contain root SKILL.md".into())
            })?;
        if metadata.size > super::MAX_CONFIG_BYTES {
            return Err(DiscoveryError::Invalid("SKILL.md exceeds 1 MiB".into()));
        }
        let fallback = PathBuf::from(&upload.config_id.name).join("SKILL.md");
        let parsed = parse_skill(
            &fallback,
            &upload.tar_data[metadata.start..metadata.start + metadata.size],
        )
        .map_err(DiscoveryError::Invalid)?;
        if parsed.name != upload.config_id.name {
            return Err(DiscoveryError::Invalid(
                "skill metadata name must match config_id.name".into(),
            ));
        }
        let identity = serde_json::to_vec(&upload.config_id)
            .map_err(|error| DiscoveryError::Invalid(error.to_string()))?;
        let hash = format!("{:x}", Sha256::digest(identity));
        let parent = self.data_dir.join("skill-uploads");
        tokio::fs::create_dir_all(&parent).await?;
        let parent = tokio::fs::canonicalize(parent).await?;
        let destination = parent.join(format!("{hash}-{:016x}", rand::random::<u64>()));
        let mut builder = tokio::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&destination).await?;
        let mut staging = UploadStaging(Some(destination.clone()));
        let result = write_entries(&destination, &entries, &upload.tar_data).await;
        if let Err(error) = result {
            cleanup_upload(&destination).await;
            staging.0 = None;
            return Err(error);
        }
        staging.0 = None;
        Ok(SkillRegistration {
            config_id: upload.config_id.clone(),
            name: parsed.name,
            description: parsed.description,
            disable_model_invocation: parsed.disable_model_invocation,
            skill_location: SkillLocation::Global(destination),
        })
    }
}

async fn write_entries(
    root: &Path,
    entries: &[UploadEntry],
    data: &[u8],
) -> Result<(), DiscoveryError> {
    // No archive links are accepted, and every file is create-only. Conflicting
    // file/ancestor paths fail without ever escaping this fresh directory.
    for entry in entries {
        let path = root.join(&entry.path);
        if entry.directory {
            tokio::fs::create_dir_all(&path).await?;
        } else {
            tokio::fs::create_dir_all(path.parent().unwrap()).await?;
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .await?;
            file.write_all(&data[entry.start..entry.start + entry.size])
                .await?;
            file.flush().await?;
        }
    }
    // Apply directory modes last so read-only directories do not block extraction.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut ordered: Vec<_> = entries.iter().collect();
        ordered.sort_by_key(|entry| {
            (
                entry.directory,
                std::cmp::Reverse(entry.path.components().count()),
            )
        });
        for entry in ordered {
            tokio::fs::set_permissions(
                root.join(&entry.path),
                std::fs::Permissions::from_mode(entry.mode),
            )
            .await?;
        }
    }
    Ok(())
}

async fn cleanup_upload(root: &Path) {
    // Make restrictive archive directories traversable before walking children.
    // Never follow symlinks during cleanup.
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await;
        }
        let Ok(mut directory) = tokio::fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(entry)) = directory.next_entry().await {
            pending.push(entry.path());
        }
    }
    let _ = tokio::fs::remove_dir_all(root).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upload_reuses_metadata_parser_and_rejects_links() {
        let directory = tempfile::tempdir().unwrap();
        let registry = ConfigRegistry::new(directory.path().to_owned());
        let mut archive = tar::Builder::new(Vec::new());
        let bytes = b"---\nname: example\ndescription: useful\n---\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "SKILL.md", &bytes[..])
            .unwrap();
        let upload = NewSkill {
            config_id: psa_api::ConfigId {
                config_group: psa_api::ConfigGroup::Custom("api".into()),
                name: "example".into(),
            },
            tar_data: archive.into_inner().unwrap(),
        };
        let skill = registry.upload_skill(&upload).await.unwrap();
        let SkillLocation::Global(path) = skill.skill_location else {
            panic!()
        };
        assert_eq!(tokio::fs::read(path.join("SKILL.md")).await.unwrap(), bytes);
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/etc/passwd").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "SKILL.md", &[][..])
            .unwrap();
        assert!(validate_archive(&archive.into_inner().unwrap()).is_err());

        let oversized = vec![b'x'; super::super::MAX_CONFIG_BYTES + 1];
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(oversized.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "SKILL.md", &oversized[..])
            .unwrap();
        let oversized_upload = NewSkill {
            tar_data: archive.into_inner().unwrap(),
            ..upload
        };
        assert!(matches!(registry.upload_skill(&oversized_upload).await,
            Err(DiscoveryError::Invalid(message)) if message.contains("1 MiB")));
    }
}
