//! Docker sandbox containers running nested probes, managed through the `docker` CLI.
use psa_api::{
    Mount, ProbeId, SandboxState,
    probes::{ContainerStatus, SandboxRequest, SandboxSpec},
};
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const MANAGER_LABEL: &str = "psa.manager";
const SANDBOX_LABEL: &str = "psa.sandbox";
const CONTAINER_PROBE: &str = "/opt/psa/psa_probe";
/// Kept in the container's own filesystem: root-owned files the probe creates
/// (lock, unpacked tools) must not land in the manager's host directories.
const CONTAINER_DATA: &str = "/opt/psa/data";
const CONTAINER_CONFIG: &str = "/opt/psa/config/probe.toml";
const CONTAINER_AUTH: &str = "/opt/psa/config/auth.toml";

/// Settings a manager probe needs to configure nested probes.
#[derive(Debug, Clone)]
pub struct SandboxManager {
    pub manager_id: ProbeId,
    pub server_url: String,
    pub key: String,
    pub data_dir: PathBuf,
    pub probe_binary: PathBuf,
}

impl SandboxManager {
    /// Execute a lifecycle request, returning container states for inspections.
    pub async fn execute(&self, request: SandboxRequest) -> Result<Option<Vec<ContainerStatus>>> {
        match request {
            SandboxRequest::Create { spec } => self.create(spec).await.map(|()| None),
            SandboxRequest::Start { probe_id } => {
                let container = self.require_container(probe_id).await?;
                run_docker(["start", container.as_str()])
                    .await
                    .map(|_| None)
            }
            SandboxRequest::Stop { probe_id } => {
                let container = self.require_container(probe_id).await?;
                run_docker(["stop", container.as_str()]).await.map(|_| None)
            }
            SandboxRequest::Remove { probe_id } => self.remove(probe_id).await.map(|()| None),
            SandboxRequest::Inspect => self.inspect().await.map(Some),
        }
    }

    fn state_dir(&self, probe_id: ProbeId) -> PathBuf {
        self.data_dir.join("sandboxes").join(probe_id.to_string())
    }

    async fn create(&self, spec: SandboxSpec) -> Result<()> {
        validate_spec(&spec)?;
        if self.find_container(spec.probe_id).await?.is_some() {
            return Err(invalid("sandbox container already exists"));
        }

        let state_dir = self.state_dir(spec.probe_id);
        tokio::fs::create_dir_all(&state_dir).await?;
        let result = async {
            self.write_nested_config(&state_dir, &spec).await?;
            let args = build_create_args(self, &spec, &state_dir);
            run_docker(&args).await
        }
        .await;

        if let Err(error) = result {
            let _ = tokio::fs::remove_dir_all(&state_dir).await;
            return Err(error);
        }
        Ok(())
    }

    async fn write_nested_config(&self, state_dir: &Path, spec: &SandboxSpec) -> Result<()> {
        let mut config = toml_edit::DocumentMut::new();
        config["id"] = toml_edit::value(spec.probe_id.to_string());
        config["server_url"] = toml_edit::value(self.server_url.as_str());
        config["label"] = toml_edit::value(spec.name.as_str());
        write_private_file(&state_dir.join("probe.toml"), &config.to_string()).await?;

        let mut auth = toml_edit::DocumentMut::new();
        auth["psa"]["probe_key"] = toml_edit::value(self.key.as_str());
        write_private_file(&state_dir.join("auth.toml"), &auth.to_string()).await
    }

    async fn remove(&self, probe_id: ProbeId) -> Result<()> {
        if let Some(container) = self.find_container(probe_id).await? {
            run_docker(["rm", "-f", container.as_str()]).await?;
        }

        match tokio::fs::remove_dir_all(self.state_dir(probe_id)).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    async fn inspect(&self) -> Result<Vec<ContainerStatus>> {
        let filter = format!("label={MANAGER_LABEL}={}", self.manager_id);
        let format = format!("{{{{.Label \"{SANDBOX_LABEL}\"}}}}\t{{{{.Names}}}}\t{{{{.State}}}}");
        let output = run_docker(["ps", "-a", "--filter", &filter, "--format", &format]).await?;
        Ok(output.lines().filter_map(parse_status_line).collect())
    }

    async fn find_container(&self, probe_id: ProbeId) -> Result<Option<String>> {
        let filter = format!("label={SANDBOX_LABEL}={probe_id}");
        let output = run_docker(["ps", "-aq", "--filter", &filter]).await?;
        Ok(output.lines().next().map(str::to_owned))
    }

    async fn require_container(&self, probe_id: ProbeId) -> Result<String> {
        self.find_container(probe_id)
            .await?
            .ok_or_else(|| invalid("sandbox container does not exist"))
    }
}

/// Docker container name for a sandbox, unique across managers on one host.
pub fn container_name(spec: &SandboxSpec) -> String {
    let id = spec.probe_id.to_string();
    format!("psa-sandbox-{}-{}", spec.name, &id[..8])
}

fn build_create_args(
    manager: &SandboxManager,
    spec: &SandboxSpec,
    state_dir: &Path,
) -> Vec<String> {
    let mut args = vec![
        "create".to_owned(),
        "--name".to_owned(),
        container_name(spec),
        "--network".to_owned(),
        "host".to_owned(),
        "--init".to_owned(),
        "--label".to_owned(),
        format!("{MANAGER_LABEL}={}", manager.manager_id),
        "--label".to_owned(),
        format!("{SANDBOX_LABEL}={}", spec.probe_id),
        "--mount".to_owned(),
        build_bind(&manager.probe_binary, Path::new(CONTAINER_PROBE), false),
        "--mount".to_owned(),
        build_bind(
            &state_dir.join("probe.toml"),
            Path::new(CONTAINER_CONFIG),
            false,
        ),
        "--mount".to_owned(),
        build_bind(
            &state_dir.join("auth.toml"),
            Path::new(CONTAINER_AUTH),
            false,
        ),
    ];
    for mount in &spec.mounts {
        args.push("--mount".to_owned());
        args.push(build_bind(
            &mount.host_path,
            &mount.container_path,
            mount.writable,
        ));
    }

    args.extend([
        "--entrypoint".to_owned(),
        CONTAINER_PROBE.to_owned(),
        spec.image.clone(),
        "start".to_owned(),
        "--config".to_owned(),
        CONTAINER_CONFIG.to_owned(),
        "--data-dir".to_owned(),
        CONTAINER_DATA.to_owned(),
    ]);
    args
}

fn build_bind(source: &Path, target: &Path, writable: bool) -> String {
    let mut bind = format!(
        "type=bind,src={},dst={}",
        source.display(),
        target.display()
    );
    if !writable {
        bind.push_str(",readonly");
    }
    bind
}

fn parse_status_line(line: &str) -> Option<ContainerStatus> {
    let mut fields = line.split('\t');
    let probe_id = ProbeId(fields.next()?.parse().ok()?);
    let container_name = fields.next()?.to_owned();
    let state = match fields.next()? {
        "running" | "restarting" => SandboxState::Running,
        _ => SandboxState::Stopped,
    };
    Some(ContainerStatus {
        probe_id,
        container_name,
        state,
    })
}

fn validate_spec(spec: &SandboxSpec) -> Result<()> {
    if !psa_api::sandboxes::is_valid_sandbox_name(&spec.name) {
        return Err(invalid(
            "sandbox name must be 1-64 ASCII letters, digits, '-', '_' or '.'",
        ));
    }
    if spec.image.trim().is_empty() || spec.image.starts_with('-') {
        return Err(invalid("sandbox image must be a Docker image reference"));
    }
    for mount in &spec.mounts {
        validate_mount(mount)?;
    }
    Ok(())
}

fn validate_mount(mount: &Mount) -> Result<()> {
    for path in [&mount.host_path, &mount.container_path] {
        let Some(text) = path.to_str() else {
            return Err(invalid("mount paths must be UTF-8"));
        };
        if !path.is_absolute() || text.contains([',', '\0', '\n']) {
            return Err(invalid(
                "mount paths must be absolute and must not contain commas",
            ));
        }
    }
    if mount.container_path.starts_with("/opt/psa") {
        return Err(invalid(
            "mounts must not target /opt/psa inside the container",
        ));
    }
    Ok(())
}

async fn run_docker<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = tokio::process::Command::new("docker")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("cannot run docker: {error}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!("docker failed: {}", stderr.trim())).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn write_private_file(path: &Path, text: &str) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(text.as_bytes()).await?;
    file.sync_all().await?;
    Ok(())
}

fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use psa_api::probes::Uuid;

    fn manager() -> SandboxManager {
        SandboxManager {
            manager_id: ProbeId(Uuid::from_u128(1)),
            server_url: "ws://127.0.0.1:8080/probes/connect".into(),
            key: "key".into(),
            data_dir: "/data".into(),
            probe_binary: "/bin/psa_probe".into(),
        }
    }

    fn spec(mounts: Vec<Mount>) -> SandboxSpec {
        SandboxSpec {
            probe_id: ProbeId(Uuid::from_u128(0xabcdef12 << 96)),
            name: "work".into(),
            image: "ubuntu:24.04".into(),
            mounts,
        }
    }

    #[test]
    fn create_arguments_run_nested_probe_on_host_network() {
        let mounts = vec![
            Mount {
                host_path: "/src".into(),
                container_path: "/work".into(),
                writable: true,
            },
            Mount {
                host_path: "/ref".into(),
                container_path: "/ref".into(),
                writable: false,
            },
        ];
        let spec = spec(mounts);
        let args = build_create_args(&manager(), &spec, Path::new("/data/sandboxes/x"));

        assert_eq!(
            &args[..3],
            ["create", "--name", "psa-sandbox-work-abcdef12"]
        );
        assert!(args.windows(2).any(|w| w == ["--network", "host"]));
        assert!(args.contains(&"--init".to_owned()));
        assert!(args.contains(&format!("psa.sandbox={}", spec.probe_id)));
        assert!(args.contains(&format!("psa.manager={}", manager().manager_id)));
        assert!(
            args.contains(
                &"type=bind,src=/bin/psa_probe,dst=/opt/psa/psa_probe,readonly".to_owned()
            )
        );
        assert!(args.contains(
            &"type=bind,src=/data/sandboxes/x/probe.toml,dst=/opt/psa/config/probe.toml,readonly"
                .to_owned()
        ));
        assert!(
            args.contains(
                &"type=bind,src=/data/sandboxes/x/auth.toml,dst=/opt/psa/config/auth.toml,readonly"
                    .to_owned()
            )
        );
        assert!(!args.iter().any(|arg| arg.contains("dst=/opt/psa/data")));
        assert!(args.contains(&"type=bind,src=/src,dst=/work".to_owned()));
        assert!(args.contains(&"type=bind,src=/ref,dst=/ref,readonly".to_owned()));
        assert_eq!(
            &args[args.len() - 8..],
            [
                "--entrypoint",
                CONTAINER_PROBE,
                "ubuntu:24.04",
                "start",
                "--config",
                CONTAINER_CONFIG,
                "--data-dir",
                CONTAINER_DATA
            ]
        );
    }

    #[test]
    fn specs_reject_unsafe_names_images_and_mounts() {
        assert!(validate_spec(&spec(Vec::new())).is_ok());

        let mut bad = spec(Vec::new());
        bad.name = "a b".into();
        assert!(validate_spec(&bad).is_err());
        bad.name = "-x".into();
        assert!(validate_spec(&bad).is_err());

        let mut bad = spec(Vec::new());
        bad.image = "--privileged".into();
        assert!(validate_spec(&bad).is_err());

        for (host, container) in [("rel", "/a"), ("/a,b", "/a"), ("/a", "/opt/psa/data")] {
            let bad = spec(vec![Mount {
                host_path: host.into(),
                container_path: container.into(),
                writable: false,
            }]);
            assert!(validate_spec(&bad).is_err(), "{host} -> {container}");
        }
    }

    #[test]
    fn status_lines_map_docker_states() {
        let id = ProbeId(Uuid::from_u128(5));
        let running = parse_status_line(&format!("{id}\tpsa-sandbox-a\trunning")).unwrap();
        assert_eq!(running.state, SandboxState::Running);
        let stopped = parse_status_line(&format!("{id}\tpsa-sandbox-a\texited")).unwrap();
        assert_eq!(stopped.state, SandboxState::Stopped);
        assert!(parse_status_line("not-a-uuid\tname\trunning").is_none());
    }
}
