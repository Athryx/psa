//! Control a local server without relying on reusable process IDs.
use crate::ConnectionSettings;
use std::{io, path::PathBuf};

#[derive(Debug, Clone)]
pub enum LocalServerStatus {
    Stopped,
    Running { connection: ConnectionSettings },
}

pub struct LocalServerManager {
    executable: PathBuf,
    data_dir: PathBuf,
}

impl LocalServerManager {
    pub fn new(executable: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            executable,
            data_dir,
        }
    }

    #[cfg(unix)]
    async fn request(&self, command: &str) -> io::Result<Option<ConnectionSettings>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = match tokio::net::UnixStream::connect(
            self.data_dir.join("server.control/socket"),
        )
        .await
        {
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
        let path = self.data_dir.join("server.operation.lock");
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
        let mut child = tokio::process::Command::new(&self.executable)
            .arg("start")
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "local server exited during startup: {status}"
                )));
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
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "local server startup timed out",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
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
