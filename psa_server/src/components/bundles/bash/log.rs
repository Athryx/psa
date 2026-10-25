use super::{BashError, ProbeHandle};
use psa_api::probes::{Command, Event, ExitStatus};
use psa_shared::probe_transport::Call;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HELPER_OUTPUT_BYTES: usize = 8192;

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
                    if output.len() + bytes.len() > MAX_HELPER_OUTPUT_BYTES {
                        return Err(BashError::Other(
                            "log helper returned excessive output".into(),
                        ));
                    }
                    output.extend(bytes);
                }
                Event::Complete { status } => return complete_shell_helper(status, output),
                Event::Cancelled => return Err(BashError::Unavailable),
                _ => {}
            }
        }
    };
    tokio::time::timeout(IO_TIMEOUT, operation)
        .await
        .map_err(|_| BashError::Other("remote log operation timed out".into()))?
}

fn complete_shell_helper(
    status: Option<ExitStatus>,
    output: Vec<u8>,
) -> Result<Vec<u8>, BashError> {
    if status
        .as_ref()
        .is_some_and(|status| status.code == Some(0) && status.signal.is_none())
    {
        return Ok(output);
    }

    let text = String::from_utf8_lossy(&output);
    let text = crate::prompts::truncate_middle(&text, MAX_HELPER_OUTPUT_BYTES);
    Err(BashError::Other(format!(
        "remote log operation failed (status: {status:?}): {text}"
    )))
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

/// Disposable log cleanup must not replace a command's result with a deletion error.
pub(super) async fn delete_best_effort(probe: &ProbeHandle, path: &Path) {
    if let Err(error) = delete(probe, path).await {
        tracing::warn!(path = %path.display(), %error, "could not delete disposable bash log");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_helper_requires_an_explicit_normal_zero_exit() {
        let output = b"helper output".to_vec();
        let status = Some(ExitStatus {
            code: Some(0),
            signal: None,
        });
        assert_eq!(
            complete_shell_helper(status, output.clone()).unwrap(),
            output
        );

        for status in [
            None,
            Some(ExitStatus {
                code: None,
                signal: None,
            }),
            Some(ExitStatus {
                code: Some(1),
                signal: None,
            }),
            Some(ExitStatus {
                code: Some(-1),
                signal: None,
            }),
            Some(ExitStatus {
                code: None,
                signal: Some(9),
            }),
            Some(ExitStatus {
                code: Some(0),
                signal: Some(9),
            }),
        ] {
            let expected_status = format!("{status:?}");
            let error = complete_shell_helper(status, output.clone())
                .unwrap_err()
                .to_string();
            assert!(error.contains(&expected_status), "{error}");
            assert!(error.ends_with("helper output"), "{error}");
        }
    }

    #[test]
    fn shell_helper_failure_bounds_lossy_output_and_preserves_both_ends() {
        let mut output = b"head-".to_vec();
        output.resize(MAX_HELPER_OUTPUT_BYTES - 5, 0xff);
        output.extend_from_slice(b"-tail");
        let error = complete_shell_helper(None, output).unwrap_err().to_string();

        assert!(error.contains("status: None"));
        assert!(error.contains("head-"));
        assert!(error.ends_with("-tail"));
        assert!(error.len() <= MAX_HELPER_OUTPUT_BYTES + 128);
    }
}
