//! Cooperative locking for shared local TOML updates. The returned file releases
//! its lock on drop; lock acquisition runs outside the async runtime workers.
use std::{io, path::Path};

pub async fn lock(path: &Path) -> io::Result<std::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(path.with_extension("lock"))
        .await?
        .into_std()
        .await;
    tokio::task::spawn_blocking(move || {
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(io::Error::other)?
}
