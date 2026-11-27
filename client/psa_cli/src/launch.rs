//! Hand the terminal to another PSA executable.
use crate::CliError;
use std::process::ExitCode;

/// Replace this process with `command` on Unix, so signals and the exit status
/// belong to the launched executable. Elsewhere, wait for it and mirror its status.
pub async fn run_foreground(mut command: tokio::process::Command) -> Result<ExitCode, CliError> {
    let program = command.as_std().get_program().to_owned();
    let describe = |error: std::io::Error| {
        std::io::Error::new(
            error.kind(),
            format!("cannot run {}: {error}", program.display()),
        )
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(describe(command.as_std_mut().exec()).into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status().await.map_err(describe)?;
        let code = status.code().and_then(|code| u8::try_from(code).ok());
        Ok(code.map_or(ExitCode::FAILURE, ExitCode::from))
    }
}
