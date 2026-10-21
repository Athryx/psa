//! PSA credentials share auth.toml with provider credentials.
use crate::providers::AuthError;
use rand::RngCore;
use std::path::Path;
use tokio::io::AsyncWriteExt;

pub async fn ensure_probe_key(path: &Path) -> Result<String, AuthError> {
    let _lock = psa_api::local_config::lock(path).await?;
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| AuthError::Invalid)?;
    if let Some(psa) = document.get("psa") {
        let table = psa.as_table().ok_or(AuthError::Invalid)?;
        if let Some(key) = table.get("probe_key") {
            return key
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or(AuthError::Invalid);
        }
    }

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    if document.get("psa").is_none() {
        document["psa"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    document["psa"]["probe_key"] = toml_edit::value(&key);

    let temporary = path.with_extension(format!("{}.tmp", psa_api::probes::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        let mut file = options.open(&temporary).await?;
        file.write_all(document.to_string().as_bytes()).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temporary, path).await
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generates_stable_key_and_preserves_provider_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        let original = "# provider comment\n[openrouter]\napi_key = 'sentinel'\n";
        tokio::fs::write(&path, original).await.unwrap();
        let first = ensure_probe_key(&path).await.unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(ensure_probe_key(&path).await.unwrap(), first);
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.starts_with(original));
        assert!(
            crate::providers::ProviderRegistry::load(&path)
                .await
                .is_ok()
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(&path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for invalid in [
            "[psa]\nprobe_key = ''",
            "[psa]\nprobe_key = 42",
            "secret = 'sentinel",
        ] {
            tokio::fs::write(&path, invalid).await.unwrap();
            let error = ensure_probe_key(&path).await.unwrap_err();
            assert!(!format!("{error:?}").contains("sentinel"));
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), invalid);
        }
    }
}
