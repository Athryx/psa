use super::{Hook, HookError};
use crate::db::NewEvent;
use crate::{
    agents::agent_context::AgentContext, components::cancellation::Cancellation, db::DbError,
    probes::ProbeHandle,
};
use psa::api::{
    events::{EventType, Log, Message},
    probes::Event,
};
use psa_shared::probe_transport::ProbeError;
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

const CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 4 * MAX_FILE_BYTES;

/// Loads Pi-style project instructions from the probe's working-directory ancestors.
pub struct ProjectContextHook;

impl Hook for ProjectContextHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        let directories = ancestor_directories(&context.session().working_directory)?;
        let probe = context.probe()?;
        let loaded = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = load_project_context(probe, &directories) => result?,
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }

        let mut events: Vec<_> = loaded
            .warnings
            .into_iter()
            .map(|message| {
                NewEvent::new(EventType::Log(Log {
                    error: false,
                    message,
                }))
            })
            .collect();
        if let Some(prompt) = format_project_context(&loaded.files) {
            events.push(NewEvent::new(EventType::Message(Message::system(
                prompt,
                "Project context",
            ))));
        }
        if !events.is_empty() {
            context.save_events(&events).await?;
        }
        Ok(())
    }
}

fn ancestor_directories(cwd: &Path) -> Result<Vec<PathBuf>, DbError> {
    if !cwd.is_absolute() {
        return Err(DbError::InvalidInput(
            "project context requires an absolute working directory".into(),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in cwd.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component),
        }
    }
    let mut directories: Vec<_> = normalized.ancestors().map(Path::to_owned).collect();
    directories.reverse();
    Ok(directories)
}

#[derive(Default)]
struct ProjectContext {
    files: Vec<(PathBuf, String)>,
    warnings: Vec<String>,
}

impl ProjectContext {
    fn record_warning(&mut self, path: &Path, message: impl std::fmt::Display) {
        self.warnings.push(format!(
            "Project context warning ({}): {message}",
            path.display()
        ));
    }
}

async fn load_project_context(
    probe: &ProbeHandle,
    directories: &[PathBuf],
) -> Result<ProjectContext, ProbeError> {
    let mut loaded = ProjectContext::default();
    let mut total_bytes = 0;
    for directory in directories {
        let entries = match probe.list_dir(directory).await {
            Ok(entries) => entries,
            Err(ProbeError::Remote { code, message }) => {
                loaded.record_warning(directory, format!("remote {code}: {message}"));
                continue;
            }
            Err(error) => return Err(error),
        };
        for candidate in CANDIDATES {
            if !entries.iter().any(|entry| {
                entry.name == candidate && matches!(entry.kind.as_str(), "file" | "symlink")
            }) {
                continue;
            }

            let path = directory.join(candidate);
            let bytes = match read_context_file(probe, &path).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    loaded.record_warning(&path, "file exceeds 1 MiB");
                    continue;
                }
                Err(ProbeError::Remote { code, message }) => {
                    loaded.record_warning(&path, format!("remote {code}: {message}"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => {
                    loaded.record_warning(&path, format!("invalid UTF-8: {error}"));
                    continue;
                }
            };
            if text.len() > MAX_CONTEXT_BYTES - total_bytes {
                loaded.record_warning(&path, "project context exceeds 4 MiB total");
                break;
            }
            total_bytes += text.len();
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
            loaded.files.push((path, text));
            break;
        }
    }
    Ok(loaded)
}

async fn read_context_file(
    probe: &ProbeHandle,
    path: &Path,
) -> Result<Option<Vec<u8>>, ProbeError> {
    let mut call = probe.read_file(path).await?;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        match call.next_event().await? {
            Event::Data {
                stream: 1,
                bytes: chunk,
            } if !eof => {
                if chunk.len() > MAX_FILE_BYTES - bytes.len() {
                    // Dropping the call cancels the remote read.
                    return Ok(None);
                }
                bytes.extend(chunk);
            }
            Event::Eof { stream: 1 } if !eof => eof = true,
            Event::Complete { status: None } if eof => return Ok(Some(bytes)),
            _ => {
                return Err(ProbeError::Protocol(
                    "unexpected context file response".into(),
                ));
            }
        }
    }
}

fn format_project_context(files: &[(PathBuf, String)]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut sections = vec!["Project-specific instructions and guidelines:".to_owned()];
    for (path, content) in files {
        sections.push(format!(
            "<project_instructions path=\"{}\">\n{content}\n</project_instructions>",
            path.display()
        ));
    }
    Some(format!(
        "<project_context>\n{}\n</project_context>",
        sections.join("\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_cover_root_through_normalized_cwd() {
        assert_eq!(
            ancestor_directories(Path::new("/work/repo/../project/./src")).unwrap(),
            ["/", "/work", "/work/project", "/work/project/src"].map(PathBuf::from)
        );
        assert_eq!(
            ancestor_directories(Path::new("/../../")).unwrap(),
            [PathBuf::from("/")]
        );
        assert!(ancestor_directories(Path::new("relative")).is_err());
    }

    #[test]
    fn formatting_preserves_contents_and_empty_files() {
        assert!(format_project_context(&[]).is_none());
        let prompt = format_project_context(&[
            ("/AGENTS.md".into(), "\n<literal> & text\n".into()),
            ("/work/CLAUDE.md".into(), String::new()),
        ])
        .unwrap();
        assert_eq!(
            prompt,
            "<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/AGENTS.md\">\n\n<literal> & text\n\n</project_instructions>\n\n<project_instructions path=\"/work/CLAUDE.md\">\n\n</project_instructions>\n</project_context>"
        );
    }
}
