//! PSA credentials share auth.toml with provider credentials.
use crate::providers::AuthError;
use rand::RngCore;
use std::path::Path;
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;

pub struct AuthKeys {
    pub probe_key: String,
    pub client_key: String,
}

pub async fn ensure_keys(path: &Path) -> Result<AuthKeys, AuthError> {
    let _lock = psa_shared::local_config::lock(path).await?;
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };

    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| AuthError::Invalid)?;
    let table = document
        .get("psa")
        .map(|psa| psa.as_table().ok_or(AuthError::Invalid))
        .transpose()?;

    // Validate both existing credentials before generating or writing either key.
    let probe_key = parse_key(table, "probe_key")?;
    let client_key = parse_key(table, "client_key")?;
    let needs_probe_key = probe_key.is_none();
    let needs_client_key = client_key.is_none();

    let keys = AuthKeys {
        probe_key: probe_key.unwrap_or_else(generate_key),
        client_key: client_key.unwrap_or_else(generate_key),
    };
    if !needs_probe_key && !needs_client_key {
        return Ok(keys);
    }

    if document.get("psa").is_none() {
        document["psa"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    // Leave existing items untouched, including their comments and formatting.
    if needs_probe_key {
        document["psa"]["probe_key"] = toml_edit::value(&keys.probe_key);
    }
    if needs_client_key {
        document["psa"]["client_key"] = toml_edit::value(&keys.client_key);
    }

    write_document(path, &document).await?;
    Ok(keys)
}

/// Atomically replace a locked auth document with owner-only permissions.
pub(crate) async fn write_document(
    path: &Path,
    document: &toml_edit::DocumentMut,
) -> Result<(), AuthError> {
    let temporary = path.with_extension(format!("{}.tmp", psa::api::probes::Uuid::new_v4()));
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
    Ok(())
}

pub(crate) fn has_bearer_key(headers: &axum::http::HeaderMap, key: &str) -> bool {
    if key.trim().is_empty() {
        return false;
    }

    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|supplied| bool::from(supplied.as_bytes().ct_eq(key.as_bytes())))
}

pub(crate) fn validate_existing_keys(document: &toml_edit::DocumentMut) -> Result<(), AuthError> {
    let table = document
        .get("psa")
        .map(|psa| psa.as_table().ok_or(AuthError::Invalid))
        .transpose()?;
    parse_key(table, "probe_key")?;
    parse_key(table, "client_key")?;
    Ok(())
}

fn parse_key(table: Option<&toml_edit::Table>, name: &str) -> Result<Option<String>, AuthError> {
    table
        .and_then(|table| table.get(name))
        .map(|key| {
            key.as_str()
                .filter(|key| !key.trim().is_empty())
                .map(str::to_owned)
                .ok_or(AuthError::Invalid)
        })
        .transpose()
}

fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_key_requires_exact_header_and_credential() {
        use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

        let mut headers = HeaderMap::new();
        assert!(!has_bearer_key(&headers, "secret"));
        for (value, expected) in [
            ("Bearer secret", true),
            ("Bearer wrong!", false),
            ("Bearer secre", false),
            ("Bearer secret-extra", false),
            ("Bearer Secret", false),
            ("bearer secret", false),
            ("Basic secret", false),
            ("secret", false),
            ("Bearer", false),
            ("Bearer ", false),
            ("Bearer  secret", false),
            ("Bearer secret ", false),
        ] {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(has_bearer_key(&headers, "secret"), expected);
        }
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
        );
        assert!(!has_bearer_key(&headers, "secret"));
        for key in ["", " ", "\t"] {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
            );
            assert!(!has_bearer_key(&headers, key));
        }
    }

    fn assert_generated_key(key: &str) {
        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    async fn assert_saved_keys(path: &Path, keys: &AuthKeys) {
        let contents = tokio::fs::read_to_string(path).await.unwrap();
        let document = contents.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            document["psa"]["probe_key"].as_str(),
            Some(keys.probe_key.as_str())
        );
        assert_eq!(
            document["psa"]["client_key"].as_str(),
            Some(keys.client_key.as_str())
        );
    }

    #[tokio::test]
    async fn generates_independent_stable_keys_for_missing_and_empty_files() {
        let directory = tempfile::tempdir().unwrap();
        for filename in ["missing.toml", "empty.toml"] {
            let path = directory.path().join(filename);
            if filename == "empty.toml" {
                tokio::fs::write(&path, "").await.unwrap();
            }
            let first = ensure_keys(&path).await.unwrap();
            assert_generated_key(&first.probe_key);
            assert_generated_key(&first.client_key);
            assert_ne!(first.probe_key, first.client_key);
            assert_saved_keys(&path, &first).await;
            let contents = tokio::fs::read_to_string(&path).await.unwrap();
            let second = ensure_keys(&path).await.unwrap();
            assert_eq!(first.probe_key, second.probe_key);
            assert_eq!(first.client_key, second.client_key);
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
        }
    }

    #[tokio::test]
    async fn preserves_provider_content_and_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        let original = "# provider comment\n[openrouter]\napi_key = 'sentinel' # credential\n";
        for suffix in [
            "",
            "\n# PSA comment\n[psa] # table comment\nother = 'untouched' # extra\n",
        ] {
            let contents = format!("{original}{suffix}");
            tokio::fs::write(&path, &contents).await.unwrap();
            let keys = ensure_keys(&path).await.unwrap();
            assert_saved_keys(&path, &keys).await;
            let updated = tokio::fs::read_to_string(&path).await.unwrap();
            assert!(updated.starts_with(&contents));
            assert!(
                crate::providers::ProviderRegistry::load(&path)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn generates_only_missing_key_and_preserves_existing_key_verbatim() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        for existing_name in ["probe_key", "client_key"] {
            let original = format!("# auth\n[psa]\n{existing_name} = ' existing key ' # keep\n");
            tokio::fs::write(&path, &original).await.unwrap();
            let keys = ensure_keys(&path).await.unwrap();
            let (existing, generated) = if existing_name == "probe_key" {
                (&keys.probe_key, &keys.client_key)
            } else {
                (&keys.client_key, &keys.probe_key)
            };
            assert_eq!(existing, " existing key ");
            assert_generated_key(generated);
            assert_saved_keys(&path, &keys).await;
            assert!(
                tokio::fs::read_to_string(&path)
                    .await
                    .unwrap()
                    .starts_with(&original)
            );
        }
    }

    #[tokio::test]
    async fn existing_keys_leave_file_and_permissions_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        let original =
            "# auth\n[psa]\nprobe_key = 'probe' # first\nclient_key = 'client' # second\n";
        tokio::fs::write(&path, original).await.unwrap();
        let before = tokio::fs::metadata(&path).await.unwrap();
        let keys = ensure_keys(&path).await.unwrap();
        assert_eq!(keys.probe_key, "probe");
        assert_eq!(keys.client_key, "client");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        let after = tokio::fs::metadata(&path).await.unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(before.permissions(), after.permissions());
    }

    #[tokio::test]
    async fn rejects_invalid_keys_before_writing_and_does_not_leak_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        for name in ["probe_key", "client_key"] {
            let other = if name == "probe_key" {
                "client_key"
            } else {
                "probe_key"
            };
            for value in ["''", "'  '", "\"\\t\"", "42", "false", "[]", "{}"] {
                for sibling in [String::new(), format!("{other} = 'SECRET_SENTINEL'\n")] {
                    let original = format!("# SECRET_SENTINEL\n[psa]\n{name} = {value}\n{sibling}");
                    tokio::fs::write(&path, &original).await.unwrap();
                    let error = ensure_keys(&path)
                        .await
                        .err()
                        .expect("invalid key accepted");
                    assert!(matches!(error, AuthError::Invalid));
                    assert!(!format!("{error:?}").contains("SECRET_SENTINEL"));
                    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
                }
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_documents_without_modifying_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        for original in [
            "secret = 'SECRET_SENTINEL",
            "psa = 'SECRET_SENTINEL'",
            "psa = { probe_key = 'SECRET_SENTINEL' }",
            "[[psa]]\nprobe_key = 'SECRET_SENTINEL'",
            "[psa]\nprobe_key = 'SECRET_SENTINEL'\nprobe_key = 'duplicate'",
        ] {
            tokio::fs::write(&path, original).await.unwrap();
            let error = ensure_keys(&path)
                .await
                .err()
                .expect("invalid document accepted");
            assert!(matches!(error, AuthError::Invalid));
            assert!(!format!("{error:?}").contains("SECRET_SENTINEL"));
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_calls_share_one_complete_pair() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        for original in [None, Some("[psa]\nprobe_key = 'existing' # keep\n")] {
            if let Some(original) = original {
                tokio::fs::write(&path, original).await.unwrap();
            }
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..24 {
                let path = path.clone();
                tasks.spawn(async move { ensure_keys(&path).await.unwrap() });
            }
            let first = tasks.join_next().await.unwrap().unwrap();
            while let Some(result) = tasks.join_next().await {
                let keys = result.unwrap();
                assert_eq!(keys.probe_key, first.probe_key);
                assert_eq!(keys.client_key, first.client_key);
            }
            assert_saved_keys(&path, &first).await;
            if original.is_some() {
                assert_eq!(first.probe_key, "existing");
            }
            let mut entries = tokio::fs::read_dir(directory.path()).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                assert_ne!(
                    entry.path().extension().and_then(|ext| ext.to_str()),
                    Some("tmp")
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_credentials_are_private_for_new_and_existing_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        for original in [None, Some("[psa]\nprobe_key = 'existing'\n")] {
            if let Some(original) = original {
                tokio::fs::write(&path, original).await.unwrap();
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                    .await
                    .unwrap();
            }
            ensure_keys(&path).await.unwrap();
            for secured_path in [&path, &path.with_extension("lock")] {
                assert_eq!(
                    tokio::fs::metadata(secured_path)
                        .await
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
    }

    #[tokio::test]
    async fn propagates_read_errors_without_replacing_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        tokio::fs::create_dir(&path).await.unwrap();
        let error = ensure_keys(&path).await.err().expect("directory accepted");
        assert!(matches!(error, AuthError::Io(_)));
        assert!(tokio::fs::metadata(&path).await.unwrap().is_dir());
    }
}
