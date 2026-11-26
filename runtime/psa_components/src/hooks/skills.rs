use super::{Hook, HookError};
use crate::prompts::skills::{SkillDescription, format_skills};
use psa::api::{
    events::{EventType, Log, Message},
    probes::Event,
};
use psa_agents::{agent_context::AgentContext, cancellation::Cancellation, probes::ProbeHandle};
use psa_shared::probe_transport::ProbeError;
use psa_store::NewEvent;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_SKILL_BYTES: usize = 1024 * 1024;

pub struct SkillsHook;

impl Hook for SkillsHook {
    fn name(&self) -> &'static str {
        "Skills"
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let paths = context.definition().skills.clone();
        let mut catalogue = SkillCatalogue::default();
        for configured in paths {
            if cancellation.is_cancelled() {
                return Ok(());
            }

            let path = resolve_skill_path(&context.session().working_directory, &configured);
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                result = read_skill_file(context.probe()?, &path) => result,
            };
            let parsed = match result {
                Ok(bytes) => parse_skill(&path, &bytes),
                Err(ReadError::Invalid(message)) => Err(message),
                Err(ReadError::Probe(ProbeError::Remote { code, message })) => {
                    Err(format!("remote {code}: {message}"))
                }
                Err(ReadError::Probe(error)) => return Err(error.into()),
            };
            let warning = match parsed {
                Ok(skill) => {
                    if catalogue.append(skill) {
                        None
                    } else {
                        Some("duplicate skill name; first configured skill wins".to_owned())
                    }
                }
                Err(message) => Some(message),
            };
            if let Some(message) = warning {
                context
                    .save_events(&[NewEvent::new(EventType::Log(Log {
                        error: false,
                        message: format!("Skill warning ({}): {message}", path.display()),
                    }))])
                    .await?;
            }
        }

        if cancellation.is_cancelled() {
            return Ok(());
        }
        if let Some(prompt) = format_skills(&catalogue.visible) {
            context
                .save_events(&[NewEvent::new(EventType::Message(Message::system(
                    prompt,
                    "Available skills",
                )))])
                .await?;
        }
        Ok(())
    }
}

fn resolve_skill_path(working_directory: &Path, configured: &Path) -> PathBuf {
    let path = if configured.is_absolute() {
        configured.to_owned()
    } else {
        working_directory.join(configured)
    };
    if path.extension().is_some_and(|extension| extension == "md") {
        path
    } else {
        path.join("SKILL.md")
    }
}

enum ReadError {
    Probe(ProbeError),
    Invalid(String),
}

impl From<ProbeError> for ReadError {
    fn from(error: ProbeError) -> Self {
        Self::Probe(error)
    }
}

async fn read_skill_file(probe: &ProbeHandle, path: &Path) -> Result<Vec<u8>, ReadError> {
    let mut call = probe.read_file(path).await?;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        match call.next_event().await? {
            Event::Data {
                stream: 1,
                bytes: chunk,
            } if !eof => {
                if chunk.len() > MAX_SKILL_BYTES - bytes.len() {
                    // Dropping the live call cancels the remote operation immediately.
                    return Err(ReadError::Invalid("skill file exceeds 1 MiB".into()));
                }
                bytes.extend(chunk);
            }
            Event::Eof { stream: 1 } if !eof => eof = true,
            Event::Complete { status: None } if eof => return Ok(bytes),
            _ => return Err(ProbeError::Protocol("unexpected skill file response".into()).into()),
        }
    }
}

struct ParsedSkill {
    description: SkillDescription,
    disable_model_invocation: bool,
}

fn parse_skill(path: &Path, bytes: &[u8]) -> Result<ParsedSkill, String> {
    let parsed = psa::api::parse_skill_metadata(path, bytes)?;
    Ok(ParsedSkill {
        description: SkillDescription {
            name: parsed.name,
            description: parsed.description,
            location: path.to_string_lossy().into_owned(),
        },
        disable_model_invocation: parsed.disable_model_invocation,
    })
}

#[derive(Default)]
struct SkillCatalogue {
    names: HashSet<String>,
    visible: Vec<SkillDescription>,
}

impl SkillCatalogue {
    /// Reserve names before visibility filtering, as Pi does.
    fn append(&mut self, skill: ParsedSkill) -> bool {
        if !self.names.insert(skill.description.name.clone()) {
            return false;
        }
        if !skill.disable_model_invocation {
            self.visible.push(skill.description);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ParsedSkill, String> {
        parse_skill(Path::new("/remote/fallback/SKILL.md"), text.as_bytes())
    }

    #[test]
    fn yaml_block_strings_bom_crlf_and_body_are_supported() {
        let skill = parse("\u{feff}---\r\nname: yaml\r\ndescription: |-\r\n  first & <line>\r\n  second 'line'\r\nunknown: [one, two]\r\n---\r\nbody: [not valid YAML").unwrap();
        assert_eq!(skill.description.name, "yaml");
        assert_eq!(
            skill.description.description,
            "first & <line>\nsecond 'line'"
        );
        assert!(!skill.disable_model_invocation);
        let folded = parse("---\ndescription: >-\n  first\n  second\n---\n").unwrap();
        assert_eq!(folded.description.description, "first second");
    }

    #[test]
    fn name_falls_back_to_parent_directory() {
        for name in ["", "name: ''\n"] {
            let skill = parse(&format!("---\n{name}description: useful\n---\n")).unwrap();
            assert_eq!(skill.description.name, "fallback");
            assert_eq!(skill.description.location, "/remote/fallback/SKILL.md");
        }
    }

    #[test]
    fn malformed_metadata_and_invalid_utf8_are_rejected() {
        for text in [
            "no frontmatter",
            "---\ndescription: unclosed\n",
            "---\nname: missing-description\n---\n",
            "---\ndescription: '  '\n---\n",
            "---\ndescription: [not, a, string]\n---\n",
            "---\ndescription: 123\n---\n",
            "---\ndescription: true\n---\n",
            "---\ndescription: null\n---\n",
            "---\ndescription: [broken\n---\n",
            "---\ndescription: useful\ndisable-model-invocation: 'true'\n---\n",
        ] {
            assert!(parse(text).is_err(), "accepted {text:?}");
        }
        assert!(parse_skill(Path::new("/skill.md"), &[0xff]).is_err());
    }

    #[test]
    fn hidden_skills_reserve_duplicate_names_and_order_is_stable() {
        let mut catalogue = SkillCatalogue::default();
        for (name, hidden, accepted) in [
            ("hidden", true, true),
            ("second", false, true),
            ("hidden", false, false),
            ("first", false, true),
            ("second", false, false),
        ] {
            let skill = parse(&format!(
                "---\nname: {name}\ndescription: useful\ndisable-model-invocation: {hidden}\n---\n"
            ))
            .unwrap();
            assert_eq!(catalogue.append(skill), accepted);
        }
        assert_eq!(
            catalogue
                .visible
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        let mut hidden = SkillCatalogue::default();
        hidden.append(
            parse("---\ndescription: hidden\ndisable-model-invocation: true\n---\n").unwrap(),
        );
        assert!(format_skills(&hidden.visible).is_none());
    }

    #[test]
    fn explicit_paths_resolve_against_remote_working_directory() {
        let cwd = Path::new("/remote/work");
        for (configured, expected) in [
            ("skill", "/remote/work/skill/SKILL.md"),
            ("skill.md", "/remote/work/skill.md"),
            ("/absolute/skill", "/absolute/skill/SKILL.md"),
            ("/absolute/skill.md", "/absolute/skill.md"),
            ("not-markdown.txt", "/remote/work/not-markdown.txt/SKILL.md"),
        ] {
            assert_eq!(
                resolve_skill_path(cwd, Path::new(configured)),
                Path::new(expected)
            );
        }
    }
}
