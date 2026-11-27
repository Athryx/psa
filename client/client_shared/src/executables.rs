//! Locate the PSA executables a local client launches.
//!
//! An explicit path wins, then the executable's environment variable, then a
//! file beside the running executable (an install directory or `target/debug`),
//! then `PATH`.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executable {
    Server,
    Desktop,
    Probe,
}

impl Executable {
    pub fn name(self) -> &'static str {
        match self {
            Self::Server => "psa_server",
            Self::Desktop => "psa_desktop",
            Self::Probe => "psa_probe",
        }
    }

    /// Environment variable naming this executable's path.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Server => "PSA_SERVER_BIN",
            Self::Desktop => "PSA_DESKTOP_BIN",
            Self::Probe => "PSA_PROBE_BIN",
        }
    }

    fn file_name(self) -> String {
        format!("{}{}", self.name(), std::env::consts::EXE_SUFFIX)
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "cannot find {name}: not beside this executable or on PATH; set {env_var} or pass its path"
)]
pub struct ExecutableNotFound {
    pub name: &'static str,
    pub env_var: &'static str,
}

/// Resolve `executable`, using `explicit` unchanged when it is given.
pub async fn resolve_executable(
    executable: Executable,
    explicit: Option<PathBuf>,
) -> Result<PathBuf, ExecutableNotFound> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os(executable.env_var()).filter(|value| !value.is_empty()) {
        return Ok(path.into());
    }

    let file_name = executable.file_name();
    if let Some(path) = find_beside_current_exe(&file_name).await {
        return Ok(path);
    }
    if let Some(path) = find_on_path(&file_name, std::env::var_os("PATH")).await {
        return Ok(path);
    }

    Err(ExecutableNotFound {
        name: executable.name(),
        env_var: executable.env_var(),
    })
}

async fn find_beside_current_exe(file_name: &str) -> Option<PathBuf> {
    let current = tokio::fs::canonicalize(std::env::current_exe().ok()?)
        .await
        .ok()?;
    let candidate = current.parent()?.join(file_name);
    is_executable_file(&candidate).await.then_some(candidate)
}

async fn find_on_path(file_name: &str, path: Option<OsString>) -> Option<PathBuf> {
    for directory in std::env::split_paths(&path?) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(file_name);
        if is_executable_file(&candidate).await {
            return Some(candidate);
        }
    }
    None
}

async fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
#[path = "tests/executables/tests.rs"]
mod tests;
