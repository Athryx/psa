//! Private local control socket. The lifetime lock prevents stale socket cleanup
//! from ever targeting another running server; no PID is used for shutdown.
#[cfg(unix)]
mod unix {
    use std::{
        io,
        os::unix::fs::{FileTypeExt, PermissionsExt},
        path::{Path, PathBuf},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    pub struct LocalControl {
        _lock: std::fs::File,
        path: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl LocalControl {
        pub fn acquire(dir: &Path) -> io::Result<std::fs::File> {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("server.lifetime.lock"))?;
            lock.try_lock()?;
            Ok(lock)
        }

        pub fn start(
            lock: std::fs::File,
            dir: &Path,
            address: std::net::SocketAddr,
            key: &str,
        ) -> io::Result<(Self, tokio::sync::oneshot::Receiver<()>)> {
            let control_dir = dir.join("server.control");
            std::fs::create_dir_all(&control_dir)?;
            std::fs::set_permissions(&control_dir, std::fs::Permissions::from_mode(0o700))?;
            let path = control_dir.join("socket");
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&path)?,
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "local control path is not a socket",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
            let listener = tokio::net::UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            let response =
                serde_json::json!({"api_url": format!("http://{address}"), "client_key": key})
                    .to_string()
                    + "\n";
            let (send, receive) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let mut stop = Some(send);
                while let Ok((stream, _)) = listener.accept().await {
                    let response = response.clone();
                    let mut stop_request = false;
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        reader.read_line(&mut line),
                    )
                    .await
                    .is_ok_and(|r| r.is_ok())
                    {
                        if matches!(line.trim(), "status" | "stop") {
                            stop_request = line.trim() == "stop";
                            let _ = reader.get_mut().write_all(response.as_bytes()).await;
                        }
                    }
                    if stop_request {
                        if let Some(stop) = stop.take() {
                            let _ = stop.send(());
                        }
                    }
                }
            });
            Ok((
                Self {
                    _lock: lock,
                    path,
                    task,
                },
                receive,
            ))
        }
    }
    impl Drop for LocalControl {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
#[cfg(unix)]
pub use unix::LocalControl;
