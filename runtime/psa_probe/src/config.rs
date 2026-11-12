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
    /// Allow the server to create Docker sandboxes on this machine.
    #[serde(default)]
    supports_sandboxes: bool,
    /// Static probe executable mounted into sandboxes; defaults to this executable.
    sandbox_probe_path: Option<PathBuf>,
}

// Ignore unrelated server fields: only the root host marker affects the probe.
#[derive(Deserialize)]
struct HostConfig {
    #[serde(default)]
    host: bool,
}

pub struct ResolvedConfig {
    pub metadata: ProbeMetadata,
    pub server_url: String,
    pub key: String,
    /// Static probe executable mounted into sandboxes, when sandboxes are supported.
    pub sandbox_probe_path: Option<PathBuf>,
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
        self.resolve_with_server_config(None).await
    }

    /// Resolve normally, but read server authentication and host marker beside
    /// the selected server configuration rather than beside probe.toml.
    pub async fn resolve_with_server_config(
        self,
        server_config: Option<&Path>,
    ) -> Result<ResolvedConfig, ConfigError> {
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
        let _lock = psa_shared::local_config::lock(&path).await?;
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
        let key_path = server_config.unwrap_or(&path).with_file_name("auth.toml");
        let host_path = server_config
            .map(Path::to_owned)
            .unwrap_or_else(|| path.with_file_name("config.toml"));
        let key = read_probe_key(&key_path).await?;
        let host = read_host_marker(&host_path).await?;

        let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
            .await?
            .trim()
            .to_owned();
        let sandbox_probe_path = if config.supports_sandboxes {
            Some(resolve_sandbox_probe_path(config.sandbox_probe_path).await?)
        } else {
            None
        };

        let metadata = ProbeMetadata {
            host,
            sandboxes: config.supports_sandboxes,
            id: config.id,
            hostname,
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            label: config.label,
            data_dir,
            home_dir: valid_home_dir(std::env::var_os("HOME")),
        };
        metadata
            .validate()
            .map_err(|_| ConfigError::Invalid("invalid probe metadata"))?;

        Ok(ResolvedConfig {
            metadata,
            server_url: url,
            key,
            sandbox_probe_path,
        })
    }
}

// An in-process probe (for example the server's local probe) is not a static
// probe binary, so it must name the packaged executable explicitly.
async fn resolve_sandbox_probe_path(configured: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
    let path = match configured {
        Some(path) => path,
        None => {
            let current = std::env::current_exe()?;
            let is_probe = current
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("psa_probe"));
            if !is_probe {
                return Err(ConfigError::Invalid(
                    "supports_sandboxes requires sandbox_probe_path in probe.toml \
                     when the probe runs inside another executable",
                ));
            }
            current
        }
    };

    let path = tokio::fs::canonicalize(path).await?;
    if path.to_str().is_none_or(|text| text.contains([',', '\n'])) {
        return Err(ConfigError::Invalid(
            "sandbox_probe_path must be UTF-8 without commas",
        ));
    }
    Ok(path)
}

fn valid_home_dir(value: Option<std::ffi::OsString>) -> Option<String> {
    let home = value?.into_string().ok()?;
    if home.len() > 4096 || home.contains('\0') || !Path::new(&home).is_absolute() {
        return None;
    }
    Some(home)
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

async fn read_host_marker(path: &Path) -> Result<bool, ConfigError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    let config: HostConfig =
        toml::from_str(&text).map_err(|_| ConfigError::Invalid("invalid config.toml"))?;
    Ok(config.host)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn home_dir_is_optional_and_validated_without_affecting_startup() {
        assert_eq!(valid_home_dir(None), None);
        assert_eq!(
            valid_home_dir(Some("/home/a b".into())).as_deref(),
            Some("/home/a b")
        );
        assert_eq!(valid_home_dir(Some("".into())), None);
        assert_eq!(valid_home_dir(Some("relative".into())), None);
        assert_eq!(
            valid_home_dir(Some(format!("/{}", "a".repeat(4096)).into())),
            None
        );
        assert_eq!(
            valid_home_dir(Some(std::ffi::OsString::from_vec(vec![b'/', 0xff]))),
            None
        );
    }
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
