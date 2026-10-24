//! Shared SKILL.md frontmatter parsing for discovery and prompt loading.
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
}

#[derive(Deserialize)]
struct Metadata {
    name: Option<String>,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

pub fn parse_skill(path: &Path, bytes: &[u8]) -> Result<ParsedSkill, String> {
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
    let metadata: Metadata = serde_yaml_ng::from_value(value)
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
    Ok(ParsedSkill {
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
        let parsed = parse_skill(Path::new("/skills/fallback/SKILL.md"), b"\xef\xbb\xbf---\r\ndescription: useful\r\ndisable-model-invocation: true\r\n---\r\nbody").unwrap();
        assert_eq!(parsed.name, "fallback");
        assert!(parsed.disable_model_invocation);
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
                parse_skill(Path::new("/skills/test/SKILL.md"), text.as_bytes()).is_err(),
                "{text}"
            );
        }
    }
}
