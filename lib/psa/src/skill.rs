//! Skills packaged from directories, uploaded into a context's config group.
use crate::{
    Error, Result,
    api::{Archive, archive::build_directory_tar, parse_skill_metadata},
};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

/// A skill packaged from a directory holding a `SKILL.md` and its support
/// files. Registering it uploads it into the context's group under its name,
/// replacing a skill of the same name unless the archive is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    name: String,
    description: String,
    archive: Arc<[u8]>,
    archive_sha256: String,
}

impl Skill {
    /// Package the skill in `directory`. The name comes from the `SKILL.md`
    /// metadata, else the directory name. Symlinks are followed; symlink cycles
    /// and special files are rejected. The archive is built now, so later edits
    /// to the directory are not picked up.
    pub async fn from_path(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let invalid = |reason: String| Error::InvalidSkill {
            path: directory.to_owned(),
            reason,
        };

        let root = tokio::fs::canonicalize(directory)
            .await
            .map_err(|error| invalid(error.to_string()))?;
        let manifest = root.join("SKILL.md");
        let bytes = tokio::fs::read(&manifest)
            .await
            .map_err(|error| invalid(format!("SKILL.md: {error}")))?;
        let metadata = parse_skill_metadata(&manifest, &bytes).map_err(invalid)?;

        let tar = build_directory_tar(&root).await.map_err(invalid)?;
        // Hashing and compressing a large archive is CPU-bound.
        let (archive, archive_sha256) = tokio::task::spawn_blocking(move || {
            let archive_sha256 = format!("{:x}", Sha256::digest(&tar));
            Ok::<_, std::io::Error>((Archive::compress_tar(&tar)?.tar_data, archive_sha256))
        })
        .await
        .map_err(|error| invalid(error.to_string()))?
        .map_err(|error| invalid(format!("compressing archive: {error}")))?;

        Ok(Self {
            name: metadata.name,
            description: metadata.description,
            archive: archive.into(),
            archive_sha256,
        })
    }

    /// Skill name, also its config name in the context's group.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Description from the `SKILL.md` metadata.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Zstd-compressed tar archive of the skill directory.
    pub fn archive(&self) -> &[u8] {
        &self.archive
    }

    /// Lowercase hex SHA-256 of the uncompressed tar archive.
    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }
}

/// A skill an agent layout includes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRef {
    /// A registered skill, looked up in the context's group, then the global group.
    Named(String),
    /// A skill uploaded into the context's group before the layout is registered.
    Skill(Skill),
}

impl SkillRef {
    /// Name the skill is included by.
    pub fn name(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Skill(skill) => skill.name(),
        }
    }
}

impl From<Skill> for SkillRef {
    fn from(skill: Skill) -> Self {
        Self::Skill(skill)
    }
}

impl From<&str> for SkillRef {
    fn from(name: &str) -> Self {
        Self::Named(name.to_owned())
    }
}

impl From<String> for SkillRef {
    fn from(name: String) -> Self {
        Self::Named(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, path::PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psa-skill-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn archive_files(skill: &Skill) -> Vec<(String, bool, Vec<u8>)> {
        let tar = zstd::bulk::decompress(skill.archive(), 1 << 20).unwrap();
        let mut archive = tar::Archive::new(tar.as_slice());
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let directory = entry.header().entry_type().is_dir();
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).unwrap();
                (path, directory, contents)
            })
            .collect()
    }

    #[tokio::test]
    async fn skills_are_packaged_reproducibly_following_symlinks() {
        let dir = temp_dir("package");
        let shared = temp_dir("shared");
        let root = dir.join("fallback-name");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("SKILL.md"), "---\ndescription: useful\n---\nbody").unwrap();
        std::fs::write(root.join("scripts/run.sh"), "echo hi").unwrap();
        std::fs::write(shared.join("notes.txt"), "shared").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&shared, root.join("linked")).unwrap();

        let skill = Skill::from_path(&root).await.unwrap();
        let again = Skill::from_path(&root).await.unwrap();

        assert_eq!(skill.name(), "fallback-name");
        assert_eq!(skill.description(), "useful");
        assert_eq!(skill.archive_sha256(), again.archive_sha256());
        let files = archive_files(&skill);
        let paths: Vec<_> = files.iter().map(|(path, ..)| path.as_str()).collect();
        #[cfg(unix)]
        assert_eq!(
            paths,
            [
                "SKILL.md",
                "linked",
                "linked/notes.txt",
                "scripts",
                "scripts/run.sh"
            ]
        );
        assert!(
            files
                .iter()
                .any(|(path, directory, contents)| path == "scripts/run.sh"
                    && !directory
                    && contents == b"echo hi")
        );

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(shared).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_cycles_and_bad_metadata_are_rejected() {
        let dir = temp_dir("invalid");
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: looped\ndescription: x\n---\n",
        )
        .unwrap();
        std::fs::create_dir(dir.join("nested")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("nested/back")).unwrap();

        let error = Skill::from_path(&dir).await.unwrap_err();
        assert!(matches!(&error, Error::InvalidSkill { reason, .. } if reason.contains("cycle")));

        std::fs::remove_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "no frontmatter").unwrap();
        assert!(matches!(
            Skill::from_path(&dir).await,
            Err(Error::InvalidSkill { .. })
        ));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
