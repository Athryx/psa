//! Desktop platform services. The native renderer is an opt-in build feature so
//! configuration and shared GUI tests can run without system WebView libraries.
use client_shared::{
    DesktopSettings,
    local::{LocalServerManager, LocalServerStatus},
};
use psa_gui::{GuiPlatform, SettingsFlush};
use std::{
    cell::RefCell,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot determine the PSA data directory; specify --data-dir")]
    MissingDataDirectory,
    #[error("cannot access desktop settings at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    // TOML diagnostics may include the secret-bearing source line. Never expose
    // the decoder's error text, including through Debug or an error source.
    #[error("invalid desktop settings at {0}")]
    Invalid(PathBuf),
    #[error("cannot serialize desktop settings")]
    Serialize,
}

pub fn resolve_settings_path(data_dir: Option<PathBuf>) -> Result<PathBuf, SettingsError> {
    let directory = match data_dir {
        Some(directory) => directory,
        None => psa_config::paths::default_data_dir().ok_or(SettingsError::MissingDataDirectory)?,
    };
    Ok(directory.join(psa_config::paths::DESKTOP_SETTINGS_FILE))
}

/// Missing settings use defaults without touching server/probe configuration.
/// Malformed existing settings are never replaced automatically.
pub async fn load_settings(path: &Path) -> Result<DesktopSettings, SettingsError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DesktopSettings::default());
        }
        Err(source) => {
            return Err(SettingsError::Io {
                path: path.into(),
                source,
            });
        }
    };
    toml::from_str(&text).map_err(|_| SettingsError::Invalid(path.into()))
}

/// Serializes writes within this desktop instance and replaces the entire file
/// atomically. Each temporary file is private from its first creation.
pub struct DesktopPlatform {
    path: PathBuf,
    writes: tokio::sync::Mutex<()>,
    local_server: LocalServerManager,
    settings_flush: RefCell<Option<SettingsFlush>>,
    /// Closes the window the way its close button does; set by the window.
    quit: RefCell<Option<std::rc::Rc<dyn Fn()>>>,
}

impl DesktopPlatform {
    pub fn new(path: PathBuf) -> Self {
        Self::with_server_binary(path, PathBuf::from("psa_server"))
    }

    pub fn with_server_binary(path: PathBuf, executable: PathBuf) -> Self {
        let data_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        Self {
            path,
            writes: tokio::sync::Mutex::new(()),
            // The server listens where config.toml says, like one started by
            // `psa`, so remote probes and clients find it at a stable address.
            local_server: LocalServerManager::new(executable, data_dir),
            settings_flush: RefCell::new(None),
            quit: RefCell::new(None),
        }
    }

    pub fn set_quit_handler(&self, quit: Option<std::rc::Rc<dyn Fn()>>) {
        *self.quit.borrow_mut() = quit;
    }

    pub fn settings_path(&self) -> &Path {
        &self.path
    }

    /// Await the GUI's latest durable snapshot before allowing a native close.
    /// Clone the callback before awaiting: it may unregister itself on unmount.
    pub async fn flush_pending_settings(&self) -> Result<(), String> {
        let flush = self.settings_flush.borrow().clone();
        match flush {
            Some(flush) => flush().await,
            None => Ok(()),
        }
    }

    pub async fn save(&self, settings: &DesktopSettings) -> Result<(), SettingsError> {
        let contents = toml::to_string_pretty(settings).map_err(|_| SettingsError::Serialize)?;
        let _guard = self.writes.lock().await;
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| SettingsError::Io {
                path: parent.into(),
                source,
            })?;
        let temporary = parent.join(format!(
            ".desktop-{}-{:016x}.tmp",
            std::process::id(),
            rand::random::<u64>()
        ));
        let result = self
            .write_and_replace(&temporary, contents.as_bytes())
            .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    async fn write_and_replace(
        &self,
        temporary: &Path,
        contents: &[u8],
    ) -> Result<(), SettingsError> {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let result = async {
            let mut file = options.open(temporary).await?;
            file.write_all(contents).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(temporary, &self.path).await
        }
        .await;
        result.map_err(|source| SettingsError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

impl GuiPlatform for DesktopPlatform {
    fn set_settings_flush(&self, flush: Option<SettingsFlush>) {
        *self.settings_flush.borrow_mut() = flush;
    }

    fn request_quit(&self) -> Result<(), String> {
        let quit = self.quit.borrow().clone();
        let quit = quit.ok_or("The window is not ready to close yet.")?;
        quit();
        Ok(())
    }

    fn local_server_status(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async move {
            self.local_server
                .status()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn local_server_start(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async move {
            self.local_server
                .start()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn local_server_stop(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async move {
            self.local_server
                .stop()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn local_server_restart(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<LocalServerStatus, String>> + '_>> {
        Box::pin(async move {
            self.local_server
                .restart()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn save_settings(
        &self,
        settings: DesktopSettings,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async move {
            self.save(&settings)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_settings_use_defaults_and_do_not_create_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desktop.toml");
        let settings = load_settings(&path).await.unwrap();
        assert!(!settings.active_profile().connection.api_url.is_empty());
        assert!(!tokio::fs::try_exists(path).await.unwrap());
    }

    #[tokio::test]
    async fn settings_round_trip_and_atomic_replacement_preserve_other_configs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/desktop.toml");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        let auth_path = path.with_file_name("auth.toml");
        tokio::fs::write(&auth_path, "server-auth-stays-unchanged")
            .await
            .unwrap();
        let platform = DesktopPlatform::new(path.clone());
        let mut settings = DesktopSettings::default();
        let profile = client_shared::ConnectionProfile::new_remote(
            "Remote",
            client_shared::ConnectionSettings {
                api_url: "https://example.invalid".into(),
                client_key: "secret-client-key".into(),
            },
        );
        settings.active_profile_id = profile.id.clone();
        settings.profiles.push(profile);
        platform.save(&settings).await.unwrap();
        let loaded = load_settings(&path).await.unwrap();
        assert_eq!(
            loaded.active_profile().connection.client_key,
            "secret-client-key"
        );
        settings.active_profile_mut().connection.api_url = "https://example.invalid/prefix".into();
        platform.save(&settings).await.unwrap();
        assert_eq!(
            load_settings(&path)
                .await
                .unwrap()
                .active_profile()
                .connection
                .api_url,
            settings.active_profile().connection.api_url
        );
        assert_eq!(
            tokio::fs::read_to_string(auth_path).await.unwrap(),
            "server-auth-stays-unchanged"
        );
        let mut entries = tokio::fs::read_dir(path.parent().unwrap()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name());
        }
        names.sort();
        assert_eq!(names, ["auth.toml", "desktop.toml"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(path)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn session_filters_with_global_projects_save_and_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desktop.toml");
        let platform = DesktopPlatform::new(path.clone());
        let mut settings = DesktopSettings::default();
        let filter = &mut settings.active_profile_mut().workspace.session_filter;
        filter.project_ids = vec![None, Some(psa::api::ProjectId("examples".into()))];
        filter.probe_ids = vec![psa::api::ProbeId(psa::api::probes::Uuid::from_u128(7))];

        platform.save(&settings).await.unwrap();

        let loaded = load_settings(&path).await.unwrap();
        assert_eq!(
            loaded.active_profile().workspace.session_filter,
            settings.active_profile().workspace.session_filter
        );
    }

    #[tokio::test]
    async fn invalid_settings_are_redacted_and_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("desktop.toml");
        let malformed = "[connection]\nclient_key = \"super-secret\" trailing-invalid\n";
        tokio::fs::write(&path, malformed).await.unwrap();
        let error = load_settings(&path).await.unwrap_err();
        assert!(!format!("{error:?} {error}").contains("super-secret"));
        assert_eq!(tokio::fs::read_to_string(path).await.unwrap(), malformed);
    }

    #[tokio::test]
    async fn split_workspace_restores_from_toml() {
        use client_shared::{SplitAxis, TranscriptTarget};
        let directory = tempfile::tempdir().unwrap();
        let platform = DesktopPlatform::new(directory.path().join("desktop.toml"));
        let mut settings = DesktopSettings::default();
        let server = settings.ensure_current_server();
        let target: TranscriptTarget =
            serde_json::from_str(r#"{"session_id":1,"agent_id":2}"#).unwrap();
        server.workspace.open(target, true);
        let second = server
            .workspace
            .split(server.workspace.focused_group, SplitAxis::Horizontal)
            .unwrap();
        server.workspace.focused_group = second;
        let other: TranscriptTarget =
            serde_json::from_str(r#"{"session_id":3,"agent_id":4}"#).unwrap();
        server.workspace.open(other, true);
        platform.save(&settings).await.unwrap();
        let mut loaded = load_settings(platform.settings_path()).await.unwrap();
        let restored = loaded.ensure_current_server();
        restored.workspace.restore();
        assert_eq!(restored.workspace.groups().len(), 2);
        assert_eq!(restored.workspace.active_tab().unwrap().target, other);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_server_uses_configured_binary_and_data_directory() {
        let directory = tempfile::tempdir().unwrap();
        let platform = DesktopPlatform::with_server_binary(
            directory.path().join("desktop.toml"),
            directory.path().join("missing-server"),
        );
        assert!(matches!(
            platform.local_server_status().await.unwrap(),
            LocalServerStatus::Stopped
        ));
        assert!(platform.local_server_start().await.is_err());
        assert!(directory.path().join("server.operation.lock").exists());
    }

    #[tokio::test]
    async fn close_flush_uses_latest_registration_and_releases_borrow_before_await() {
        use std::{cell::Cell, rc::Rc};

        let directory = tempfile::tempdir().unwrap();
        let platform = Rc::new(DesktopPlatform::new(directory.path().join("desktop.toml")));
        platform.flush_pending_settings().await.unwrap();
        platform.set_settings_flush(Some(Rc::new(|| {
            Box::pin(async { panic!("replaced callback must not run") })
        })));
        let called = Rc::new(Cell::new(false));
        let called_by_flush = called.clone();
        let weak_platform = Rc::downgrade(&platform);
        platform.set_settings_flush(Some(Rc::new(move || {
            let platform = weak_platform.upgrade().unwrap();
            let called = called_by_flush.clone();
            Box::pin(async move {
                tokio::task::yield_now().await;
                platform.set_settings_flush(None);
                called.set(true);
                Ok(())
            })
        })));
        platform.flush_pending_settings().await.unwrap();
        assert!(called.get());
        called.set(false);
        platform.flush_pending_settings().await.unwrap();
        assert!(!called.get(), "unmounted callback is no longer invoked");
    }

    #[tokio::test]
    async fn close_flush_waits_for_completion_and_propagates_failure_for_retry() {
        use std::{cell::Cell, rc::Rc};
        use tokio::sync::Notify;

        let directory = tempfile::tempdir().unwrap();
        let platform = DesktopPlatform::new(directory.path().join("desktop.toml"));
        let started = Rc::new(Notify::new());
        let release = Rc::new(Notify::new());
        let complete = Rc::new(Cell::new(false));
        let started_by_flush = started.clone();
        let release_by_flush = release.clone();
        platform.set_settings_flush(Some(Rc::new(move || {
            let started = started_by_flush.clone();
            let release = release_by_flush.clone();
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Err("save failed".into())
            })
        })));
        let flush = async {
            let result = platform.flush_pending_settings().await;
            complete.set(true);
            result
        };
        let driver = async {
            started.notified().await;
            assert!(!complete.get(), "native close must await the flush");
            release.notify_one();
        };
        let (result, ()) = tokio::join!(flush, driver);
        assert_eq!(result, Err("save failed".into()));
        platform.set_settings_flush(Some(Rc::new(|| Box::pin(async { Ok(()) }))));
        platform.flush_pending_settings().await.unwrap();
    }

    #[test]
    fn explicit_directory_only_affects_desktop_path() {
        assert_eq!(
            resolve_settings_path(Some(PathBuf::from("/custom/psa"))).unwrap(),
            Path::new("/custom/psa/desktop.toml")
        );
    }
}
