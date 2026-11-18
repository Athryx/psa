//! Registered skills and skill uploads.

use crate::api::{ConfigId, ProbeId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Stable mutable skill record identity, not a content revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub u64);

/// Trusted filesystem source of a skill's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLocation {
    /// Server-owned skill source, materialized on probes before use.
    Global(PathBuf),
    /// Skill source already owned by a probe.
    Probe {
        /// Probe holding the source.
        id: ProbeId,
        /// Probe-local skill path.
        path: PathBuf,
    },
}

/// Registered mutable skill metadata and its trusted source location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Stable record identity retained across updates.
    pub id: SkillId,
    /// Named configuration identity used for inclusion.
    pub config_id: ConfigId,
    /// Skill name parsed from its metadata.
    pub name: String,
    /// Skill description parsed from its metadata.
    pub description: String,
    /// Filesystem source used for materialization.
    pub skill_location: SkillLocation,
    /// Exclude from the model-facing skill catalogue; defaults to false.
    #[serde(default)]
    pub disable_model_invocation: bool,
    /// Lowercase hex SHA-256 of the uploaded tar archive; absent for skills
    /// discovered on the filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
}

/// Upload a skill archive rather than asserting a server-local path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSkill {
    /// Destination identity; its name must match the skill metadata name.
    pub config_id: ConfigId,
    /// Tar archive containing a root `SKILL.md` and support files, compressed as
    /// `compression` says. Base64-encoded on the wire.
    #[serde(with = "base64_bytes")]
    pub tar_data: Vec<u8>,
    /// Compression applied to `tar_data`; defaults to none.
    #[serde(default)]
    pub compression: ArchiveCompression,
}

/// Compression of an uploaded archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveCompression {
    /// A plain tar archive.
    #[default]
    None,
    /// A zstd-compressed tar archive.
    Zstd,
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

/// Metadata read from a `SKILL.md` frontmatter block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    /// Skill name; the containing directory's name when the metadata has none.
    pub name: String,
    /// Required nonempty description.
    pub description: String,
    /// Exclude from the model-facing skill catalogue.
    pub disable_model_invocation: bool,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// Parse the YAML frontmatter of the `SKILL.md` at `path`. A missing or empty
/// name falls back to the name of the directory containing `path`.
pub fn parse_skill_metadata(path: &Path, bytes: &[u8]) -> Result<SkillMetadata, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| format!("invalid UTF-8: {error}"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(|line| line.trim_end_matches(['\r', '\n'])) != Some("---") {
        return Err("missing YAML frontmatter".into());
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err("unterminated YAML frontmatter".into());
    }

    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
        .map_err(|error| format!("invalid skill metadata: {error}"))?;
    let metadata: Frontmatter = serde_yaml_ng::from_value(value)
        .map_err(|error| format!("invalid skill metadata: {error}"))?;
    if metadata.description.trim().is_empty() {
        return Err("description is required and must not be empty".into());
    }

    let name = metadata
        .name
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    if name.trim().is_empty() {
        return Err("skill name is required".into());
    }
    Ok(SkillMetadata {
        name,
        description: metadata.description,
        disable_model_invocation: metadata.disable_model_invocation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_and_directory_fallback() {
        let parsed = parse_skill_metadata(Path::new("/skills/fallback/SKILL.md"), b"\xef\xbb\xbf---\r\ndescription: useful\r\ndisable-model-invocation: true\r\n---\r\nbody").unwrap();
        assert_eq!(parsed.name, "fallback");
        assert!(parsed.disable_model_invocation);
    }

    #[test]
    fn uploads_encode_archives_as_base64() {
        let upload = NewSkill {
            config_id: ConfigId {
                config_group: crate::api::ConfigGroup::Global,
                name: "x".into(),
            },
            tar_data: vec![0, 1, 255],
            compression: ArchiveCompression::Zstd,
        };

        let value = serde_json::to_value(&upload).unwrap();
        assert_eq!(value["tar_data"], "AAH/");
        assert_eq!(value["compression"], "Zstd");
        let decoded: NewSkill = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.tar_data, upload.tar_data);

        let mut plain = serde_json::to_value(&upload).unwrap();
        plain.as_object_mut().unwrap().remove("compression");
        let decoded: NewSkill = serde_json::from_value(plain).unwrap();
        assert_eq!(decoded.compression, ArchiveCompression::None);
    }

    #[test]
    fn rejects_invalid_metadata() {
        for text in [
            "no header",
            "---\ndescription: text",
            "---\ndescription: 12\n---",
            "---\ndescription: ''\n---",
            "---\ndescription: text\ndisable-model-invocation: 'true'\n---",
        ] {
            assert!(
                parse_skill_metadata(Path::new("/skills/test/SKILL.md"), text.as_bytes()).is_err(),
                "{text}"
            );
        }
    }
}
