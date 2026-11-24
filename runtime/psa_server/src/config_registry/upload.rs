use super::{ConfigRegistry, DiscoveryError};
use psa::api::{NewSkill, SkillLocation, parse_skill_metadata};
use psa_shared::archive::{self, ArchiveError};
use psa_store::SkillRegistration;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

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

const SKILL_ARCHIVE_LIMITS: archive::Limits = archive::Limits {
    max_unpacked_bytes: super::MAX_SKILL_ARCHIVE_BYTES,
    max_entries: 10_000,
};

fn map_archive_error(error: ArchiveError) -> DiscoveryError {
    match error {
        ArchiveError::Invalid(message) => DiscoveryError::Invalid(format!("skill {message}")),
        ArchiveError::TooLarge(_) => {
            DiscoveryError::Invalid("skill archive exceeds 256 MiB uncompressed".into())
        }
        ArchiveError::Io(error) => DiscoveryError::Io(error),
    }
}

impl ConfigRegistry {
    /// Remove only a managed API upload, never global/project source directories.
    /// Call after replacing its DB registration, or rolling back a failed upsert.
    pub async fn remove_uploaded_source(
        &self,
        location: &SkillLocation,
    ) -> Result<(), DiscoveryError> {
        let _guard = self.gate.write().await;
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
        let entries = archive::read_entries(&upload.archive, SKILL_ARCHIVE_LIMITS)
            .await
            .map_err(map_archive_error)?;
        let _guard = self.gate.write().await;
        let metadata = entries
            .entries()
            .iter()
            .find(|entry| entry.path == Path::new("SKILL.md") && !entry.directory)
            .ok_or_else(|| {
                DiscoveryError::Invalid("skill archive must contain root SKILL.md".into())
            })?;
        let metadata = entries.contents(metadata);
        if metadata.len() > super::MAX_CONFIG_BYTES {
            return Err(DiscoveryError::Invalid("SKILL.md exceeds 1 MiB".into()));
        }
        let fallback = PathBuf::from(&upload.config_id.name).join("SKILL.md");
        let parsed = parse_skill_metadata(&fallback, metadata).map_err(DiscoveryError::Invalid)?;
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
        // The destination is fresh, so extraction cannot merge with or escape into
        // anything else. A failed extraction removes what it created; the
        // staging directory itself is removed here.
        let result = archive::extract_into(&entries, &destination).await;
        if let Err(error) = result {
            cleanup_upload(&destination).await;
            staging.0 = None;
            return Err(map_archive_error(error));
        }
        staging.0 = None;
        Ok(SkillRegistration {
            config_id: upload.config_id.clone(),
            name: parsed.name,
            description: parsed.description,
            disable_model_invocation: parsed.disable_model_invocation,
            skill_location: SkillLocation::Global(destination),
            archive_sha256: Some(format!("{:x}", Sha256::digest(entries.tar()))),
        })
    }
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
    use psa::api::{Archive, ArchiveCompression};

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
            config_id: psa::api::ConfigId {
                config_group: psa::api::ConfigGroup::Custom("api".into()),
                name: "example".into(),
            },
            archive: Archive {
                tar_data: archive.into_inner().unwrap(),
                compression: ArchiveCompression::None,
            },
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
        let linked = NewSkill {
            archive: Archive {
                tar_data: archive.into_inner().unwrap(),
                compression: ArchiveCompression::None,
            },
            ..upload.clone()
        };
        assert!(matches!(registry.upload_skill(&linked).await,
            Err(DiscoveryError::Invalid(message)) if message.contains("files and directories")));

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
            archive: Archive {
                tar_data: archive.into_inner().unwrap(),
                compression: ArchiveCompression::None,
            },
            ..upload
        };
        assert!(matches!(registry.upload_skill(&oversized_upload).await,
            Err(DiscoveryError::Invalid(message)) if message.contains("1 MiB")));
    }
}
