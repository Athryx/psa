//! Setup and teardown commands a sandbox image declares through environment
//! variables, run by the nested probe around its connection to the server.
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncWriteExt;

/// Shell command run on every container start, before the probe connects.
pub const SETUP_VAR: &str = "PSA_SETUP";
/// Shell command run when the container stops.
pub const TEARDOWN_VAR: &str = "PSA_TEARDOWN";
/// Whole seconds the teardown command may run before it is killed.
pub const TEARDOWN_TIMEOUT_VAR: &str = "PSA_TEARDOWN_TIMEOUT";
/// Hook variables, kept out of processes the probe spawns for agents.
pub const HOOK_VARS: [&str; 3] = [SETUP_VAR, TEARDOWN_VAR, TEARDOWN_TIMEOUT_VAR];

pub const DEFAULT_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(300);

const LOG_FILE: &str = "image-hooks.log";

/// Hook commands read from the probe's own environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageHooks {
    pub setup: Option<String>,
    pub teardown: Option<String>,
    pub teardown_timeout: Duration,
}

impl ImageHooks {
    pub fn from_env() -> Self {
        let read = |name| std::env::var(name).ok();
        Self {
            setup: read(SETUP_VAR).filter(|command| !command.trim().is_empty()),
            teardown: read(TEARDOWN_VAR).filter(|command| !command.trim().is_empty()),
            teardown_timeout: parse_teardown_timeout(read(TEARDOWN_TIMEOUT_VAR).as_deref()),
        }
    }

    /// Run setup to completion. Failures are logged rather than returned, so the
    /// probe still connects and the sandbox stays reachable for inspection.
    pub async fn run_setup(&self, data_dir: &Path) {
        let Some(command) = &self.setup else {
            return;
        };
        tracing::info!("running {SETUP_VAR}");
        report_hook(SETUP_VAR, run_hook(command, data_dir, None).await);
    }

    /// Run teardown, killing it once the teardown timeout passes.
    pub async fn run_teardown(&self, data_dir: &Path) {
        let Some(command) = &self.teardown else {
            return;
        };
        tracing::info!("running {TEARDOWN_VAR}");
        let result = run_hook(command, data_dir, Some(self.teardown_timeout)).await;
        report_hook(TEARDOWN_VAR, result);
    }
}

/// Parse a timeout in whole seconds, falling back to the default when missing
/// or invalid and capping it so a bad image cannot stall `docker stop`.
pub fn parse_teardown_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TEARDOWN_TIMEOUT)
        .min(MAX_TEARDOWN_TIMEOUT)
}

/// Find the teardown timeout among `NAME=value` image environment entries.
pub fn find_teardown_timeout<'a>(env: impl IntoIterator<Item = &'a str>) -> Duration {
    let prefix = format!("{TEARDOWN_TIMEOUT_VAR}=");
    let value = env
        .into_iter()
        .filter_map(|entry| entry.strip_prefix(prefix.as_str()))
        .last();
    parse_teardown_timeout(value)
}

fn report_hook(name: &str, result: io::Result<()>) {
    match result {
        Ok(()) => tracing::info!("{name} finished"),
        Err(error) => tracing::warn!("{name} failed: {error}"),
    }
}

/// Run `sh -c command` with output appended to the hook log. Only the shell is
/// awaited: daemons it backgrounds keep the log file open without blocking us.
async fn run_hook(command: &str, data_dir: &Path, timeout: Option<Duration>) -> io::Result<()> {
    let log_path = data_dir.join(LOG_FILE);
    let mut log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await?;
    log.write_all(format!("--- {command}\n").as_bytes()).await?;
    let log = log.into_std().await;

    let mut process = tokio::process::Command::new("/bin/sh");
    process
        .arg("-c")
        .arg(command)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for name in HOOK_VARS {
        process.env_remove(name);
    }
    let mut child = process.spawn()?;
    let pid = child
        .id()
        .map(|pid| Pid::from_raw(pid as i32))
        .ok_or_else(|| io::Error::other("hook exited before it started"))?;

    // Kill the whole group if we stop waiting, including on cancellation.
    let mut group = HookGroup { pid, armed: true };
    let status = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out after {}s", timeout.as_secs()),
                )
            })??,
        None => child.wait().await?,
    };
    group.armed = false;

    if !status.success() {
        return Err(io::Error::other(format!(
            "{status}; see {}",
            log_path.display()
        )));
    }
    Ok(())
}

struct HookGroup {
    pid: Pid,
    armed: bool,
}

impl Drop for HookGroup {
    fn drop(&mut self) {
        if self.armed {
            let _ = killpg(self.pid, Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_timeout_defaults_and_caps() {
        assert_eq!(parse_teardown_timeout(None), DEFAULT_TEARDOWN_TIMEOUT);
        assert_eq!(parse_teardown_timeout(Some("x")), DEFAULT_TEARDOWN_TIMEOUT);
        assert_eq!(parse_teardown_timeout(Some("-1")), DEFAULT_TEARDOWN_TIMEOUT);
        assert_eq!(
            parse_teardown_timeout(Some(" 30 ")),
            Duration::from_secs(30)
        );
        assert_eq!(parse_teardown_timeout(Some("0")), Duration::ZERO);
        assert_eq!(parse_teardown_timeout(Some("100000")), MAX_TEARDOWN_TIMEOUT);
    }

    #[test]
    fn teardown_timeout_found_in_image_env() {
        let env = ["PATH=/bin", "PSA_TEARDOWN_TIMEOUT=45", "PSA_TEARDOWN=true"];
        assert_eq!(find_teardown_timeout(env), Duration::from_secs(45));
        assert_eq!(
            find_teardown_timeout(["PATH=/bin"]),
            DEFAULT_TEARDOWN_TIMEOUT
        );
    }

    #[tokio::test]
    async fn hooks_log_output_and_report_failures() {
        let dir = tempfile::tempdir().unwrap();
        run_hook("echo hello", dir.path(), None).await.unwrap();
        let error = run_hook("exit 3", dir.path(), None).await.unwrap_err();
        assert!(error.to_string().contains("exit status: 3"), "{error}");

        let log = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
        assert!(log.contains("--- echo hello\nhello\n--- exit 3\n"), "{log}");
    }

    #[tokio::test]
    async fn hooks_time_out_and_do_not_wait_for_daemons() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Some(Duration::from_millis(200));
        let error = run_hook("sleep 30", dir.path(), timeout).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // A backgrounded daemon holding the log open must not block completion.
        let started = std::time::Instant::now();
        run_hook("sleep 3 &", dir.path(), None).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
