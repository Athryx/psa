use clap::Args;
use psa_api::{ProbeId, ProbeMetadata, probes::Uuid};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Args, Default)]
pub struct StartOptions {
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: ProbeId,
    server_url: Option<String>,
    label: Option<String>,
}

pub struct ResolvedConfig {
    pub metadata: ProbeMetadata,
    pub server_url: String,
    pub key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("probe configuration I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

impl StartOptions {
    pub async fn resolve(self) -> Result<ResolvedConfig, ConfigError> {
        let data_dir = self
            .data_dir
            .or_else(|| {
                directories::ProjectDirs::from("", "", "psa")
                    .map(|dirs| dirs.data_local_dir().to_owned())
            })
            .ok_or(ConfigError::Invalid(
                "cannot determine data directory; use --data-dir",
            ))?;
        tokio::fs::create_dir_all(&data_dir).await?;
        let data_dir = tokio::fs::canonicalize(data_dir).await?;

        let explicit = self.config.is_some();
        let path = self.config.unwrap_or_else(|| data_dir.join("probe.toml"));
        let _lock = psa_api::local_config::lock(&path).await?;
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                String::new()
            }
            Err(error) => return Err(error.into()),
        };
        let mut document = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| ConfigError::Invalid("invalid probe.toml"))?;

        let mut changed = false;
        if document.get("id").is_none() {
            document["id"] = toml_edit::value(Uuid::new_v4().to_string());
            changed = true;
        }
        if let Some(server) = self.server {
            document["server_url"] = toml_edit::value(server);
            changed = true;
        }
        if let Some(label) = self.label {
            document["label"] = toml_edit::value(label);
            changed = true;
        }
        let config: Config = toml::from_str(&document.to_string())
            .map_err(|_| ConfigError::Invalid("invalid probe.toml"))?;
        if changed {
            write_config(&path, &document.to_string()).await?;
        }

        let server_url = config
            .server_url
            .filter(|url| !url.trim().is_empty())
            .ok_or(ConfigError::Invalid(
                "missing server_url in probe.toml; use --server",
            ))?;
        let url = parse_server_url(&server_url)?;
        let key = read_probe_key(&path.with_file_name("auth.toml")).await?;

        let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
            .await?
            .trim()
            .to_owned();
        let metadata = ProbeMetadata {
            id: config.id,
            hostname,
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            label: config.label,
            data_dir,
        };
        metadata
            .validate()
            .map_err(|_| ConfigError::Invalid("invalid probe metadata"))?;

        Ok(ResolvedConfig {
            metadata,
            server_url: url,
            key,
        })
    }
}

fn parse_server_url(value: &str) -> Result<String, ConfigError> {
    // Parse URLs through the same URL type used by the websocket handshake.
    let url = psa_api::client::parse_probe_server_url(value).map_err(|_| {
        ConfigError::Invalid(
            "server_url must be an HTTP(S) or WS(S) URL without credentials, query, or fragment",
        )
    })?;
    Ok(url)
}

async fn read_probe_key(path: &Path) -> Result<String, ConfigError> {
    let auth = tokio::fs::read_to_string(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConfigError::Invalid("missing auth.toml; copy [psa].probe_key from the server")
        } else {
            ConfigError::Io(error)
        }
    })?;
    let auth: toml::Value =
        toml::from_str(&auth).map_err(|_| ConfigError::Invalid("invalid auth.toml"))?;
    let key = auth
        .get("psa")
        .and_then(|psa| psa.get("probe_key"))
        .and_then(toml::Value::as_str)
        .filter(|key| !key.trim().is_empty() && !key.contains(['\r', '\n']))
        .ok_or(ConfigError::Invalid(
            "missing or invalid [psa].probe_key in auth.toml",
        ))?
        .to_owned();
    Ok(key)
}

async fn write_config(path: &Path, text: &str) -> Result<(), std::io::Error> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .await?;
        file.write_all(text.as_bytes()).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temporary, path).await
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
}
