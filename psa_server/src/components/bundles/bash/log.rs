use super::{BashError, ProbeHandle};
use psa_api::{
    probe_transport::Call,
    probes::{Command, Event},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// All interpolated data is positional argv, never shell source.
async fn run_shell_helper(
    probe: &ProbeHandle,
    script: &str,
    arguments: Vec<String>,
) -> Result<Vec<u8>, BashError> {
    let operation = async {
        let mut call = probe
            .spawn(
                Command::new("sh", "/")
                    .arg("-c")
                    .arg(script)
                    .arg("psa-bash-log")
                    .args(arguments),
            )
            .await?;
        call.sender.close_stdin().await?;
        let mut output = Vec::new();
        loop {
            match call.next_event().await? {
                Event::Data { bytes, .. } => {
                    if output.len() + bytes.len() > 8192 {
                        return Err(BashError::Other(
                            "log helper returned excessive output".into(),
                        ));
                    }
                    output.extend(bytes);
                }
                Event::Complete { status } => {
                    if status.is_some_and(|s| s.code.is_some_and(|code| code != 0)) {
                        return Err(BashError::Other(format!(
                            "remote log operation failed: {}",
                            String::from_utf8_lossy(&output)
                        )));
                    }
                    return Ok(output);
                }
                Event::Cancelled => return Err(BashError::Unavailable),
                _ => {}
            }
        }
    };
    tokio::time::timeout(IO_TIMEOUT, operation)
        .await
        .map_err(|_| BashError::Other("remote log operation timed out".into()))?
}

pub(super) async fn require_working_directory(
    probe: &ProbeHandle,
    path: &Path,
) -> Result<(), BashError> {
    let output = run_shell_helper(
        probe,
        "if [ -e \"$1\" ]; then printf yes; else printf no; fi",
        vec![path.to_string_lossy().into_owned()],
    )
    .await?;
    if output == b"yes" {
        return Ok(());
    }
    Err(BashError::Other(format!(
        "Working directory does not exist: {}\nCannot execute bash commands.",
        path.display()
    )))
}

pub(super) async fn create(probe: &ProbeHandle) -> Result<PathBuf, BashError> {
    let random: [u8; 12] = rand::random();
    let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let bytes = run_shell_helper(probe,
        "umask 077; set -C; d=${TMPDIR:-/tmp}; case $d in /*) ;; *) d=/tmp ;; esac; p=$d/pi-bash-$1.log; : > \"$p\" || exit 1; printf '%s' \"$p\"",
        vec![id]).await?;
    let path = String::from_utf8(bytes)
        .map_err(|_| BashError::Other("remote temporary path is not UTF-8".into()))?;
    if !path.starts_with('/') || path.is_empty() {
        return Err(BashError::Other("invalid remote temporary path".into()));
    }
    Ok(path.into())
}

pub(super) async fn delete(probe: &ProbeHandle, path: &Path) -> Result<(), BashError> {
    run_shell_helper(
        probe,
        "rm -f -- \"$1\"",
        vec![path.to_string_lossy().into_owned()],
    )
    .await?;
    Ok(())
}

/// A completed write-file operation acknowledges sync_all, not just stream credit.
/// Each bounded process chunk is durably appended before its preview is exposed.
pub(super) async fn append(
    probe: &ProbeHandle,
    path: &Path,
    bytes: &[u8],
) -> Result<(), BashError> {
    let operation = async {
        let mut call = probe.write_file(path, true).await?;
        let sender = call.sender.clone();
        let write = async {
            sender.write(0, bytes).await?;
            sender.close_stdin().await?;
            Ok::<_, BashError>(())
        };
        tokio::try_join!(write, await_write_completion(&mut call))?;
        Ok(())
    };
    tokio::time::timeout(IO_TIMEOUT, operation)
        .await
        .map_err(|_| BashError::Other("log append timed out".into()))?
}

async fn await_write_completion(call: &mut Call) -> Result<(), BashError> {
    loop {
        match call.next_event().await? {
            Event::Complete { .. } => return Ok(()),
            Event::Cancelled => return Err(BashError::Unavailable),
            _ => {}
        }
    }
}

pub(super) fn resolve_shell(mut command: Command) -> Command {
    if command.program == "bash" {
        command.program = "sh".into();
        let original = std::mem::take(&mut command.args);
        command.args = vec!["-c".into(), "if [ -x /bin/bash ]; then exec /bin/bash \"$@\"; elif command -v bash >/dev/null 2>&1; then exec bash \"$@\"; else exec sh \"$@\"; fi".into(), "psa-bash".into()];
        command.args.extend(original);
    }
    command
}
