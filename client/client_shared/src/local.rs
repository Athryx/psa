//! Control a local server without relying on reusable process IDs.
use crate::ConnectionSettings;
use psa_config::paths;
use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// Lines of the server log quoted when startup fails.
const STARTUP_LOG_TAIL_LINES: usize = 20;

#[derive(Debug, Clone)]
pub enum LocalServerStatus {
    Stopped,
    Running { connection: ConnectionSettings },
}

pub struct LocalServerManager {
    executable: PathBuf,
    data_dir: PathBuf,
    listen: Option<SocketAddr>,
    probe_binary: Option<PathBuf>,
}

impl LocalServerManager {
    pub fn new(executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            executable,
            data_dir,
            listen: None,
            probe_binary: None,
        }
    }

    /// Listen address passed to started servers instead of the one in `config.toml`.
    pub fn with_listen(mut self, listen: Option<SocketAddr>) -> Self {
        self.listen = listen;
        self
    }

    /// Static probe executable started servers mount into sandboxes when
    /// `probe.toml` names none.
    pub fn with_probe_binary(mut self, probe_binary: Option<PathBuf>) -> Self {
        self.probe_binary = probe_binary;
        self
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Where started servers write their output.
    pub fn log_path(&self) -> PathBuf {
        paths::server_log_file(&self.data_dir)
    }

    #[cfg(unix)]
    async fn request(&self, command: &str) -> io::Result<Option<ConnectionSettings>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let socket = paths::server_control_socket(&self.data_dir);
        let mut stream = match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        stream.write_all(format!("{command}\n").as_bytes()).await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local server did not respond"))??;
        let connection = serde_json::from_str(&line).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid local server response")
        })?;
        Ok(Some(connection))
    }

    #[cfg(unix)]
    async fn operation_lock(&self) -> io::Result<std::fs::File> {
        tokio::fs::create_dir_all(&self.data_dir).await?;
        let path = self.data_dir.join(paths::SERVER_OPERATION_LOCK);
        tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(path)?;
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(io::Error::other)?
    }

    #[cfg(unix)]
    async fn is_ready(connection: &ConnectionSettings) -> bool {
        let request = reqwest::Client::new()
            .get(format!("{}/health", connection.api_url))
            .bearer_auth(&connection.client_key)
            .send();
        matches!(tokio::time::timeout(std::time::Duration::from_secs(2), request).await, Ok(Ok(response)) if response.status().is_success())
    }

    #[cfg(unix)]
    pub async fn status(&self) -> io::Result<LocalServerStatus> {
        match self.request("status").await? {
            Some(connection) if Self::is_ready(&connection).await => {
                Ok(LocalServerStatus::Running { connection })
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local server is not ready",
            )),
            None => Ok(LocalServerStatus::Stopped),
        }
    }

    #[cfg(unix)]
    async fn start_locked(&self) -> io::Result<LocalServerStatus> {
        if let Some(connection) = self.request("status").await? {
            if Self::is_ready(&connection).await {
                return Ok(LocalServerStatus::Running { connection });
            }
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local server is not ready",
            ));
        }
        let log = self.open_log().await?;
        let mut command = tokio::process::Command::new(&self.executable);
        command.arg("start").arg("--data-dir").arg(&self.data_dir);
        if let Some(listen) = self.listen {
            command.arg("--listen").arg(listen.to_string());
        }
        if let Some(probe_binary) = &self.probe_binary {
            command.arg("--probe-binary").arg(probe_binary);
        }
        // A new process group keeps the server running when the launching
        // terminal closes or receives Ctrl-C.
        let mut child = command
            .process_group(0)
            // The output goes to a file, so keep it free of terminal colors.
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot run {}: {error}", self.executable.display()),
                )
            })?;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait()? {
                let message = format!("local server exited during startup: {status}");
                return Err(self.startup_error(io::ErrorKind::Other, message).await);
            }
            match self.request("status").await {
                Ok(Some(connection)) if Self::is_ready(&connection).await => {
                    return Ok(LocalServerStatus::Running { connection });
                }
                Ok(_) => (),
                Err(error) => {
                    let _ = child.kill().await;
                    return Err(error);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill().await;
                let message = "local server startup timed out".to_owned();
                return Err(self.startup_error(io::ErrorKind::TimedOut, message).await);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Truncate the log so it holds only the output of the server being started.
    #[cfg(unix)]
    async fn open_log(&self) -> io::Result<std::fs::File> {
        let path = self.log_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .await
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot open server log {}: {error}", path.display()),
                )
            })?;
        Ok(file.into_std().await)
    }

    /// Describe a failed startup with the end of the server's log.
    #[cfg(unix)]
    async fn startup_error(&self, kind: io::ErrorKind, message: String) -> io::Error {
        let path = self.log_path();
        let log = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines[lines.len().saturating_sub(STARTUP_LOG_TAIL_LINES)..].join("\n");
        let mut message = format!("{message} (log: {})", path.display());
        if !tail.trim().is_empty() {
            message.push('\n');
            message.push_str(&tail);
        }
        io::Error::new(kind, message)
    }

    #[cfg(unix)]
    async fn stop_locked(&self) -> io::Result<LocalServerStatus> {
        if self.request("stop").await?.is_none() {
            return Ok(LocalServerStatus::Stopped);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if self.request("status").await?.is_none() {
                return Ok(LocalServerStatus::Stopped);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "local server shutdown timed out",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[cfg(unix)]
    pub async fn start(&self) -> io::Result<LocalServerStatus> {
        let _lock = self.operation_lock().await?;
        self.start_locked().await
    }
    #[cfg(unix)]
    pub async fn stop(&self) -> io::Result<LocalServerStatus> {
        let _lock = self.operation_lock().await?;
        self.stop_locked().await
    }
    #[cfg(unix)]
    pub async fn restart(&self) -> io::Result<LocalServerStatus> {
        let _lock = self.operation_lock().await?;
        self.stop_locked().await?;
        self.start_locked().await
    }

    #[cfg(not(unix))]
    pub async fn status(&self) -> io::Result<LocalServerStatus> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local server control requires Unix",
        ))
    }
    #[cfg(not(unix))]
    pub async fn start(&self) -> io::Result<LocalServerStatus> {
        self.status().await
    }
    #[cfg(not(unix))]
    pub async fn stop(&self) -> io::Result<LocalServerStatus> {
        self.status().await
    }
    #[cfg(not(unix))]
    pub async fn restart(&self) -> io::Result<LocalServerStatus> {
        self.status().await
    }
}

#[cfg(all(test, unix))]
#[path = "tests/local/tests.rs"]
mod tests;
