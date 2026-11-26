//! Remote filesystem plumbing shared by the file tools.

use crate::prompts;
use psa::api::{
    ProbeId,
    probes::{Command, Event, Stdio},
};
use psa_agents::probes::ProbeHandle;
use psa_shared::probe_transport::ProbeError;
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(super) const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

pub(super) async fn resolve_path(
    probe: &ProbeHandle,
    input: &str,
    cwd: &Path,
) -> Result<PathBuf, ProbeError> {
    let input = normalize_path_input(input);

    let home = if input == "~" || input.starts_with("~/") {
        Some(read_remote_home_directory(probe).await?)
    } else {
        None
    };

    resolve_normalized_path(&input, cwd, home.as_deref()).map_err(ProbeError::Protocol)
}

async fn read_remote_home_directory(probe: &ProbeHandle) -> Result<String, ProbeError> {
    let mut child = probe
        .spawn(
            Command::new("sh", "/")
                .args(["-c", "printf '%s' ~"])
                .stdin(Stdio::Null),
        )
        .await?;

    let mut bytes = Vec::new();
    loop {
        match child.next_event().await? {
            Event::Data {
                stream: 1,
                bytes: chunk,
            } => {
                if bytes.len() + chunk.len() > 4096 {
                    return Err(ProbeError::Protocol(
                        "remote home directory exceeds path limit".into(),
                    ));
                }
                bytes.extend(chunk);
            }
            Event::Data { stream: 2, .. } | Event::Eof { .. } | Event::Started { .. } => {}
            Event::Complete {
                status: Some(status),
            } if status.code == Some(0) => break,
            Event::Complete { .. } => {
                return Err(ProbeError::Protocol(
                    "could not resolve remote home directory".into(),
                ));
            }
            _ => {
                return Err(ProbeError::Protocol(
                    "unexpected home directory response".into(),
                ));
            }
        }
    }

    let home = String::from_utf8(bytes)
        .map_err(|_| ProbeError::Protocol("remote home directory is not UTF-8".into()))?;
    if !Path::new(&home).is_absolute() {
        return Err(ProbeError::Protocol(
            "remote home directory is not absolute".into(),
        ));
    }
    Ok(home)
}

fn normalize_path_input(input: &str) -> String {
    let input: String = input
        .chars()
        .map(|character| {
            if matches!(
                character,
                '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            ) {
                ' '
            } else {
                character
            }
        })
        .collect();

    input.strip_prefix('@').unwrap_or(&input).to_owned()
}

fn resolve_normalized_path(input: &str, cwd: &Path, home: Option<&str>) -> Result<PathBuf, String> {
    let path = if input == "~" || input.starts_with("~/") {
        let home = home.ok_or("remote home directory is unavailable")?;
        if input == "~" {
            PathBuf::from(home)
        } else {
            Path::new(home).join(&input[2..])
        }
    } else if input.starts_with("file://") {
        url::Url::parse(input)
            .map_err(|error| error.to_string())?
            .to_file_path()
            .map_err(|_| "invalid file URL".to_owned())?
    } else {
        PathBuf::from(input)
    };

    let joined = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    if !joined.is_absolute() {
        return Err("file tools require an absolute working directory".into());
    }

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component),
        }
    }
    Ok(normalized)
}

pub(super) async fn read_bytes(probe: &ProbeHandle, path: &Path) -> Result<Vec<u8>, ProbeError> {
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
                    return Err(ProbeError::Protocol(
                        "file exceeds the 64 MiB in-memory processing limit".into(),
                    ));
                }
                bytes.extend(chunk);
            }
            Event::Eof { stream: 1 } if !eof => eof = true,
            Event::Complete { status: None } if eof => return Ok(bytes),
            Event::Cancelled => return Err(ProbeError::Protocol("read cancelled".into())),
            _ => return Err(ProbeError::Protocol("unexpected file read response".into())),
        }
    }
}

pub(super) async fn write_bytes(
    probe: &ProbeHandle,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ProbeError> {
    let mut call = probe.write_file(path, false).await?;
    let sender = call.sender.clone();
    let send = async {
        sender.write(0, bytes).await?;
        sender.close_stdin().await
    };
    let receive = async {
        match call.next_event().await? {
            Event::Complete { status: None } => Ok(()),
            _ => Err(ProbeError::Protocol(
                "unexpected file write response".into(),
            )),
        }
    };
    tokio::try_join!(send, receive)?;
    Ok(())
}

// This orders PSA tool calls sharing the same lexical path. It cannot promise
// remote IO settlement after disconnection or forced cancellation, and it does
// not canonicalize symlinks: the probe protocol exposes neither guarantee.
type FileLocks = HashMap<(ProbeId, PathBuf), Weak<Mutex<()>>>;
static FILE_LOCKS: OnceLock<Mutex<FileLocks>> = OnceLock::new();

pub(super) async fn lock_file(probe_id: ProbeId, path: &Path) -> OwnedMutexGuard<()> {
    let locks = FILE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock = {
        let mut locks = locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (probe_id, path.to_owned());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };

    lock.lock_owned().await
}

pub(super) fn parse_remote_error_code(error: &ProbeError) -> Option<&'static str> {
    let ProbeError::Remote { code, message } = error else {
        return None;
    };

    for (number, name) in [
        (2, "ENOENT"),
        (13, "EACCES"),
        (1, "EPERM"),
        (21, "EISDIR"),
        (20, "ENOTDIR"),
        (17, "EEXIST"),
        (30, "EROFS"),
        (28, "ENOSPC"),
        (36, "ENAMETOOLONG"),
        (22, "EINVAL"),
    ] {
        if code == name || message.contains(&format!("(os error {number})")) {
            return Some(name);
        }
    }
    None
}

pub(super) fn format_io_error(error: &ProbeError, path: &Path, operation: &str) -> String {
    let message = match parse_remote_error_code(error) {
        Some(code) => {
            let reason = match code {
                "ENOENT" => "no such file or directory",
                "EACCES" => "permission denied",
                "EPERM" => "operation not permitted",
                "EISDIR" => "illegal operation on a directory",
                "ENOTDIR" => "not a directory",
                "EEXIST" => "file already exists",
                "EROFS" => "read-only file system",
                "ENOSPC" => "no space left on device",
                "ENAMETOOLONG" => "name too long",
                _ => "invalid argument",
            };
            if code == "EISDIR" && operation == "open" {
                format!("{code}: {reason}, read")
            } else {
                format!("{code}: {reason}, {operation} '{}'", path.display())
            }
        }
        None => match error {
            ProbeError::Remote { message, .. } => message.clone(),
            error => error.to_string(),
        },
    };
    prompts::truncate_middle(&message, 50 * 1024).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_pi_normalization_without_server_home_fallback() {
        for (input, expected) in [
            ("@dir/../hello\u{202f}world", "/work/hello world"),
            ("~/../other", "/home/other"),
            ("file:///tmp/hello%20world", "/tmp/hello world"),
            ("/../../tmp/./file", "/tmp/file"),
            (" name ", "/work/ name "),
        ] {
            let normalized = normalize_path_input(input);
            assert_eq!(
                resolve_normalized_path(&normalized, Path::new("/work"), Some("/home/probe"))
                    .unwrap(),
                PathBuf::from(expected)
            );
        }
        assert!(resolve_normalized_path("~/file", Path::new("/work"), None).is_err());
        assert!(
            resolve_normalized_path("file://other-host/tmp/file", Path::new("/work"), None)
                .is_err()
        );
    }

    #[test]
    fn known_errno_messages_use_node_format_without_disclosing_extra_details() {
        let error = ProbeError::Remote {
            code: "operation_failed".into(),
            message: "No such file or directory (os error 2)".into(),
        };
        assert_eq!(
            format_io_error(&error, Path::new("/work/missing"), "open"),
            "ENOENT: no such file or directory, open '/work/missing'"
        );
    }

    #[tokio::test]
    async fn file_locks_share_only_matching_probe_and_path() {
        let id = crate::test_support::probe_metadata().id;
        let guard = lock_file(id, Path::new("/work/file")).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                lock_file(id, Path::new("/work/file"))
            )
            .await
            .is_err()
        );
        let other = lock_file(id, Path::new("/work/other")).await;
        drop((guard, other));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                lock_file(id, Path::new("/work/file"))
            )
            .await
            .is_ok()
        );
    }
}
