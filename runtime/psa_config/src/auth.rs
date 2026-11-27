//! `auth.toml` holds the PSA probe and client keys under `[psa]` and one
//! table per LLM provider. Every writer holds the shared file lock and
//! replaces the file atomically with owner-only permissions.

use llm_api::providers::ProviderType;
use rand::RngCore;
use serde::Deserialize;
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum AuthFileError {
    #[error("cannot read auth file: {0}")]
    Io(#[from] std::io::Error),
    // Do not retain TOML errors: their source excerpts can contain credentials.
    #[error("invalid auth.toml")]
    Invalid,
}

pub struct AuthKeys {
    pub probe_key: String,
    pub client_key: String,
}

/// Load the PSA keys, generating and saving whichever is missing.
/// Existing items, comments, and formatting are preserved.
pub async fn ensure_keys(path: &Path) -> Result<AuthKeys, AuthFileError> {
    let _lock = psa_shared::local_config::lock(path).await?;
    let text = read_optional(path).await?;

    let mut document = parse_document(&text)?;
    let table = psa_table(&document)?;

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

/// Read `[psa].probe_key`, as a probe does to authenticate to its server.
/// A missing file is reported as an I/O `NotFound` error.
pub async fn read_probe_key(path: &Path) -> Result<Option<String>, AuthFileError> {
    let text = tokio::fs::read_to_string(path).await?;
    let document = parse_document(&text)?;
    parse_key(psa_table(&document)?, "probe_key")
}

/// An auth document holding only a probe key, for nested sandbox probes.
pub fn build_probe_document(probe_key: &str) -> toml_edit::DocumentMut {
    let mut psa = toml_edit::Table::new();
    psa["probe_key"] = toml_edit::value(probe_key);
    let mut document = toml_edit::DocumentMut::new();
    document["psa"] = toml_edit::Item::Table(psa);
    document
}

/// One provider's credential. API-key providers only use `token`; Codex
/// stores its ChatGPT OAuth token set so the server can refresh it.
///
/// Intentionally does not implement Debug: it holds secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderCredential {
    /// API key or OAuth access token, sent as a bearer token.
    pub token: String,
    /// OAuth refresh token. Without one an expired token must be replaced by hand.
    pub refresh_token: Option<String>,
    /// ChatGPT account (workspace) the token acts for.
    pub account_id: Option<String>,
    /// Access-token expiry, in Unix seconds.
    pub expires_at: Option<i64>,
}

impl ProviderCredential {
    /// A credential with only a token, as entered by hand.
    pub fn token(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            refresh_token: None,
            account_id: None,
            expires_at: None,
        }
    }
}

/// Replace a provider's table with `credential`, or remove it for `None`.
/// Fields the provider's table does not support are not written.
pub fn apply_provider_credential(
    document: &mut toml_edit::DocumentMut,
    provider: ProviderType,
    credential: Option<&ProviderCredential>,
) {
    let section = provider.as_str();
    let Some(credential) = credential else {
        document.remove(section);
        return;
    };

    let mut table = toml_edit::Table::new();
    match provider {
        ProviderType::Openrouter => {
            table["api_key"] = toml_edit::value(&credential.token);
        }
        ProviderType::OpenAiCodex => {
            table["access_token"] = toml_edit::value(&credential.token);
            if let Some(refresh_token) = &credential.refresh_token {
                table["refresh_token"] = toml_edit::value(refresh_token);
            }
            if let Some(account_id) = &credential.account_id {
                table["account_id"] = toml_edit::value(account_id);
            }
            if let Some(expires_at) = credential.expires_at {
                table["expires_at"] = toml_edit::value(expires_at);
            }
        }
    }
    document[section] = toml_edit::Item::Table(table);
}

/// Provider credentials present in an auth document. Unknown tables and empty
/// credentials are rejected; `[psa]` keys are not checked here.
pub fn parse_provider_credentials(
    text: &str,
) -> Result<Vec<(ProviderType, ProviderCredential)>, AuthFileError> {
    let auth: ProviderAuth = toml::from_str(text).map_err(|_| AuthFileError::Invalid)?;
    let _ = &auth.psa;
    let credentials = [
        (
            ProviderType::Openrouter,
            auth.openrouter
                .map(|auth| ProviderCredential::token(auth.api_key)),
        ),
        (
            ProviderType::OpenAiCodex,
            auth.openai_codex.map(|auth| ProviderCredential {
                token: auth.access_token,
                refresh_token: auth.refresh_token,
                account_id: auth.account_id,
                expires_at: auth.expires_at,
            }),
        ),
    ];

    let mut present = Vec::new();
    for (provider, credential) in credentials {
        let Some(credential) = credential else {
            continue;
        };
        let blank = |value: &Option<String>| value.as_ref().is_some_and(|v| v.trim().is_empty());
        if credential.token.trim().is_empty()
            || blank(&credential.refresh_token)
            || blank(&credential.account_id)
        {
            return Err(AuthFileError::Invalid);
        }
        present.push((provider, credential));
    }
    Ok(present)
}

/// Read an auth file, treating a missing file as empty.
pub async fn read_optional(path: &Path) -> Result<String, AuthFileError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn parse_document(text: &str) -> Result<toml_edit::DocumentMut, AuthFileError> {
    text.parse().map_err(|_| AuthFileError::Invalid)
}

/// Atomically replace a locked auth document with owner-only permissions.
pub async fn write_document(
    path: &Path,
    document: &toml_edit::DocumentMut,
) -> Result<(), AuthFileError> {
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

/// Check that any PSA keys already present are well formed.
pub fn validate_existing_keys(document: &toml_edit::DocumentMut) -> Result<(), AuthFileError> {
    let table = psa_table(document)?;
    parse_key(table, "probe_key")?;
    parse_key(table, "client_key")?;
    Ok(())
}

fn psa_table(
    document: &toml_edit::DocumentMut,
) -> Result<Option<&toml_edit::Table>, AuthFileError> {
    document
        .get("psa")
        .map(|psa| psa.as_table().ok_or(AuthFileError::Invalid))
        .transpose()
}

/// Read a PSA key from the `[psa]` table. Keys must be non-blank single-line
/// strings, since they are sent in HTTP headers.
fn parse_key(
    table: Option<&toml_edit::Table>,
    name: &str,
) -> Result<Option<String>, AuthFileError> {
    table
        .and_then(|table| table.get(name))
        .map(|key| {
            key.as_str()
                .filter(|key| !key.trim().is_empty() && !key.contains(['\r', '\n']))
                .map(str::to_owned)
                .ok_or(AuthFileError::Invalid)
        })
        .transpose()
}

fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderAuth {
    psa: Option<toml::Value>,
    openrouter: Option<OpenrouterAuth>,
    openai_codex: Option<CodexAuth>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenrouterAuth {
    api_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexAuth {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(parse_provider_credentials(&updated).is_ok());
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
                    assert!(matches!(error, AuthFileError::Invalid));
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
            assert!(matches!(error, AuthFileError::Invalid));
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
        assert!(matches!(error, AuthFileError::Io(_)));
        assert!(tokio::fs::metadata(&path).await.unwrap().is_dir());
    }

    #[test]
    fn codex_token_sets_round_trip_and_old_tables_still_parse() {
        let credential = ProviderCredential {
            token: "access".into(),
            refresh_token: Some("refresh".into()),
            account_id: Some("account".into()),
            expires_at: Some(1_800_000_000),
        };
        let mut document = parse_document("[psa]\nclient_key = 'c'\n").unwrap();
        apply_provider_credential(&mut document, ProviderType::OpenAiCodex, Some(&credential));
        apply_provider_credential(
            &mut document,
            ProviderType::Openrouter,
            Some(&ProviderCredential::token("key")),
        );
        let parsed = parse_provider_credentials(&document.to_string()).unwrap();
        assert!(parsed.contains(&(ProviderType::OpenAiCodex, credential)));
        assert!(parsed.contains(&(ProviderType::Openrouter, ProviderCredential::token("key"))));

        apply_provider_credential(&mut document, ProviderType::OpenAiCodex, None);
        assert_eq!(
            parse_provider_credentials(&document.to_string())
                .unwrap()
                .len(),
            1
        );

        let old = parse_provider_credentials("[openai_codex]\naccess_token = 'a'\n").unwrap();
        assert!(old[0].1 == ProviderCredential::token("a"));
        for invalid in [
            "[openai_codex]\naccess_token = 'a'\nrefresh_token = ' '\n",
            "[openrouter]\napi_key = 'a'\nrefresh_token = 'r'\n",
        ] {
            assert!(parse_provider_credentials(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn probe_key_reads_require_a_file_and_single_line_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.toml");
        let error = read_probe_key(&path).await.err().unwrap();
        assert!(
            matches!(error, AuthFileError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
        );

        tokio::fs::write(&path, "[openrouter]\napi_key = 'unused'\n")
            .await
            .unwrap();
        assert_eq!(read_probe_key(&path).await.unwrap(), None);

        let document = build_probe_document("probe");
        tokio::fs::write(&path, document.to_string()).await.unwrap();
        assert_eq!(
            read_probe_key(&path).await.unwrap().as_deref(),
            Some("probe")
        );

        tokio::fs::write(&path, "[psa]\nprobe_key = \"a\\nb\"\n")
            .await
            .unwrap();
        assert!(matches!(
            read_probe_key(&path).await,
            Err(AuthFileError::Invalid)
        ));
    }
}
