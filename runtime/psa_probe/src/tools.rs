use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncWriteExt;

pub async fn ensure_tools(data_dir: &Path) -> std::io::Result<()> {
    let bin = data_dir.join("bin");
    tokio::fs::create_dir_all(&bin).await?;
    for (name, bytes) in [
        (
            "rg",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/rg")).as_slice(),
        ),
        (
            "fd",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/fd")).as_slice(),
        ),
    ] {
        let path = bin.join(name);
        let matches = match tokio::fs::read(&path).await {
            Ok(existing) => Sha256::digest(&existing) == Sha256::digest(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };

        if !matches {
            let temporary = bin.join(format!(".{name}-{}", psa::api::probes::Uuid::new_v4()));
            let result = async {
                let mut file = tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o755)
                    .open(&temporary)
                    .await?;
                file.write_all(bytes).await?;
                file.sync_all().await?;
                tokio::fs::rename(&temporary, &path).await
            }
            .await;

            if result.is_err() {
                let _ = tokio::fs::remove_file(temporary).await;
            }
            result?;
        }
        tokio::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .await?;
    }

    let licenses = data_dir.join("licenses");
    tokio::fs::create_dir_all(&licenses).await?;
    for (name, bytes) in [
        (
            "ripgrep-COPYING",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/ripgrep-COPYING")).as_slice(),
        ),
        (
            "ripgrep-LICENSE-MIT",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/ripgrep-LICENSE-MIT")).as_slice(),
        ),
        (
            "fd-LICENSE-MIT",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/fd-LICENSE-MIT")).as_slice(),
        ),
        (
            "fd-LICENSE-APACHE",
            include_bytes!(concat!(env!("PSA_BUNDLED_ASSETS"), "/fd-LICENSE-APACHE")).as_slice(),
        ),
    ] {
        tokio::fs::write(licenses.join(name), bytes).await?;
    }
    Ok(())
}
