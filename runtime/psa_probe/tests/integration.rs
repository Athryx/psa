use psa::api::{
    Client, ProbeId,
    probes::{Command, Event, ExitStatus, Uuid},
};
use psa_probe::config::StartOptions;
use psa_server::{
    db::Database,
    probes::{ProbeHandle, ProbeRegistry},
    providers::ProviderRegistry,
};
use psa_shared::probe_transport::{Call, ProbeError};
use std::time::Duration;
use tokio::{net::TcpListener, sync::oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest},
};

#[path = "integration/bash_bundle.rs"]
mod bash_bundle;
#[path = "integration/bash_interaction.rs"]
mod bash_interaction;
#[path = "integration/discovery.rs"]
mod discovery;

struct Harness {
    dir: tempfile::TempDir,
    client: Client,
    registry: ProbeRegistry,
    id: ProbeId,
    metadata: psa::api::ProbeMetadata,
    base: String,
    probe_stop: oneshot::Sender<()>,
    probe: tokio::task::JoinHandle<Result<(), psa_probe::ProbeError>>,
    server_stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), psa_server::server::ServerError>>,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with_host(false).await
    }

    async fn start_with_host(host: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("server.sqlite3"))
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let registry = ProbeRegistry::start("test-probe-key".into());
        let (server_stop, stopped) = oneshot::channel();
        let server = tokio::spawn(psa_server::server::serve_with_configuration(
            listener,
            database,
            ProviderRegistry::default()
                .with_catalog_source(psa_server::models::CatalogSource::new(&base).unwrap()),
            registry.clone(),
            psa_server::config_registry::ConfigRegistry::new(dir.path().join("global")),
            psa_server::config::default_model_config(),
            "test-client-key".into(),
            async {
                let _ = stopped.await;
            },
        ));
        let probe_dir = dir.path().join("probe");
        tokio::fs::create_dir_all(&probe_dir).await.unwrap();
        tokio::fs::write(
            probe_dir.join("auth.toml"),
            "[psa]\nprobe_key = 'test-probe-key'\n",
        )
        .await
        .unwrap();
        if host {
            tokio::fs::write(probe_dir.join("config.toml"), "host = true\n")
                .await
                .unwrap();
        }
        let mut config = StartOptions {
            data_dir: Some(probe_dir),
            server: Some(base.clone()),
            label: Some("integration".into()),
            ..Default::default()
        }
        .resolve()
        .await
        .unwrap();
        if let Ok(home) = std::env::var("HOME") {
            if std::path::Path::new(&home).is_absolute() && home.len() <= 4096 {
                assert_eq!(config.metadata.home_dir.as_deref(), Some(home.as_str()));
            }
        }
        config.metadata.home_dir = Some("/home/test user & files".into());
        let id = config.metadata.id;
        let metadata = config.metadata.clone();
        let (probe_stop, stopped) = oneshot::channel();
        let probe = tokio::spawn(psa_probe::run(config, async {
            let _ = stopped.await;
        }));
        let harness = Self {
            client: Client::new(&base, "test-client-key").unwrap(),
            dir,
            registry,
            id,
            metadata,
            base,
            probe_stop,
            probe,
            server_stop,
            server,
        };
        harness.wait_active().await;
        harness
    }
    async fn wait_active(&self) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !self.registry.is_active(self.id).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("probe did not connect");
    }
    async fn handle(&self) -> ProbeHandle {
        self.registry.handle(self.id).await.unwrap()
    }
    async fn stop(self) {
        let _ = self.probe_stop.send(());
        self.probe.await.unwrap().unwrap();
        assert!(
            !self.registry.is_active(self.id).await || {
                tokio::time::sleep(Duration::from_millis(50)).await;
                !self.registry.is_active(self.id).await
            }
        );
        self.server_stop.send(()).unwrap();
        self.server.await.unwrap().unwrap();
    }
    fn request(&self, key: &str, version: u32) -> tungstenite::handshake::client::Request {
        let url =
            reqwest::Url::parse(&psa::api::probes::parse_connect_url(&self.base).unwrap()).unwrap();
        let mut url = url;
        url.query_pairs_mut()
            .append_pair("version", &version.to_string())
            .append_pair("id", &self.id.to_string())
            .append_pair("hostname", &self.metadata.hostname)
            .append_pair("os", "linux")
            .append_pair("architecture", "x86_64")
            .append_pair("host", if self.metadata.host { "true" } else { "false" })
            .append_pair("data_dir", self.metadata.data_dir.to_str().unwrap());
        let mut request = url.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {key}").parse().unwrap());
        request
    }
}

async fn collect(mut call: Call) -> (Vec<u8>, Vec<u8>, Option<ExitStatus>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        match call.next_event().await.unwrap() {
            Event::Data { stream: 1, bytes } => stdout.extend(bytes),
            Event::Data { stream: 2, bytes } => stderr.extend(bytes),
            Event::Complete { status } => return (stdout, stderr, status),
            Event::Eof { .. } | Event::Started { .. } => {}
            event => panic!("unexpected event {event:?}"),
        }
    }
}

#[tokio::test]
async fn probe_and_client_keys_are_isolated() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        for key in ["bad-key", "test-client-key"] {
            match connect_async(harness.request(key, psa::api::probes::PROTOCOL_VERSION)).await {
                Err(tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), 401);
                }
                result => panic!("unexpected handshake for {key}: {result:?}"),
            }
        }

        // The real probe authenticated with its own key and remains discoverable
        // using the separate client key after the rejected handshakes.
        assert!(harness.registry.is_active(harness.id).await);
        let probes = harness.client.list_probes().await.unwrap();
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].metadata.id, harness.id);
        assert_eq!(probes[0].metadata.label.as_deref(), Some("integration"));
        assert_eq!(probes[0].metadata.home_dir, harness.metadata.home_dir);
        assert!(probes[0].is_active);

        let probe_client = Client::new(&harness.base, "test-probe-key").unwrap();
        assert!(matches!(
            probe_client.list_probes().await,
            Err(psa::api::ClientError::Api { status: 401, .. })
        ));
        let health = reqwest::Client::new()
            .get(format!("{}/health", harness.base))
            .bearer_auth("test-probe-key")
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::UNAUTHORIZED);
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn registration_auth_sessions_and_reconnect() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let probes = harness.client.list_probes().await.unwrap();
        assert_eq!(probes.len(), 1);
        assert!(probes[0].is_active);
        assert_eq!(probes[0].metadata.label.as_deref(), Some("integration"));
        assert_eq!(
            harness
                .client
                .get_probe(harness.id)
                .await
                .unwrap()
                .metadata
                .data_dir,
            harness.metadata.data_dir
        );
        for (key, version, expected) in [
            ("bad-key", psa::api::probes::PROTOCOL_VERSION, 401),
            ("test-probe-key", 1, 400),
            ("test-probe-key", psa::api::probes::PROTOCOL_VERSION, 409),
        ] {
            let result = connect_async(harness.request(key, version)).await;
            match result {
                Err(tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), expected)
                }
                result => panic!("unexpected handshake {result:?}"),
            }
        }
        let session = harness
            .client
            .create_session(&psa::api::CreateSession {
                decision_model: None,
                name: "remote".into(),
                working_directory: harness.dir.path().to_owned(),
                probe_id: harness.id,
                project_id: None,
                model_config: Some(
                    serde_json::from_value(serde_json::json!({
                        "model": "openrouter/test-model",
                        "effort": "Medium"
                    }))
                    .unwrap(),
                ),
            })
            .await
            .unwrap();
        let fork = harness
            .client
            .fork_session(
                session.id,
                &psa::api::ForkSession {
                    point: psa::api::ForkPoint::Head,
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(fork.session.probe_id, harness.id);
        assert!(
            harness
                .client
                .create_session(&psa::api::CreateSession {
                    decision_model: None,
                    name: "bad".into(),
                    working_directory: "/tmp".into(),
                    probe_id: ProbeId(Uuid::new_v4()),
                    project_id: None,
                    model_config: Some(session.model_config.clone()),
                })
                .await
                .is_err()
        );
        let stale = harness.handle().await;
        harness.registry.disconnect(harness.id).await.unwrap();
        assert!(matches!(
            stale.read_file("/tmp/unused").await,
            Err(ProbeError::Disconnected)
        ));
        harness.wait_active().await;
        assert_eq!(harness.client.list_probes().await.unwrap().len(), 1);
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn startup_reads_remote_skill_metadata_from_configured_references() {
    use psa::api::events::{EventType, MessageType};

    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let handle = harness.handle().await;
        let cwd = harness.dir.path().join("remote<&>");
        for skill in ["review", "direct", "hidden"] {
            handle.create_dir(cwd.join(".agents/skills").join(skill)).await.unwrap();
        }
        for (path, contents) in [
            (
                ".agents/skills/review/SKILL.md",
                "---\nname: 'review<&>'\ndescription: 'Review <code> & tests'\n---\nSECRET DIRECTORY SKILL BODY\n",
            ),
            (
                ".agents/skills/direct/SKILL.md",
                "---\nname: direct\ndescription: A direct file\n---\nSECRET DIRECT SKILL BODY\n",
            ),
            (
                ".agents/skills/hidden/SKILL.md",
                "---\nname: hidden-skill\ndescription: hidden description\ndisable-model-invocation: true\n---\nSECRET HIDDEN SKILL BODY\n",
            ),
        ] {
            let call = handle.write_file(cwd.join(path), false).await.unwrap();
            call.sender.write(0, contents.as_bytes()).await.unwrap();
            call.sender.close_stdin().await.unwrap();
            collect(call).await;
        }
        let session = harness
            .client
            .create_session(&psa::api::CreateSession {
                decision_model: None,
                name: "skills".into(),
                working_directory: cwd.clone(),
                probe_id: harness.id,
                project_id: None,
                model_config: Some(serde_json::from_value(serde_json::json!({
                    "model": "openrouter/test-model",
                    "effort": "Medium"
                }))
                .unwrap()),
            })
            .await
            .unwrap();
        let config_id = psa::api::ConfigId {
            config_group: psa::api::ConfigGroup::Custom("test".into()),
            name: "remote skills".into(),
        };
        let project = psa::api::ConfigGroup::AgentsDir {
            path: cwd.join(".agents").to_str().unwrap().into(),
            probe_id: harness.id,
        };
        harness.client.upsert_agent_configs(&[psa::api::AgentConfig {
            config_id: config_id.clone(),
            description: String::new(),
            selectable: true,
            bundles: vec![
                psa::api::BundleConfig::Startup {
                    base_prompt: true,
                    project_context: false,
                    cwd: true,
                    skills: true,
                },
                psa::api::BundleConfig::Noop,
            ],
            models: vec![psa::api::ModelSelection::Global],
            first_model_default: true,
            subagents: Default::default(),
            skills: psa::api::IncludedItems {
                groups: vec![],
                items: ["review<&>", "direct", "hidden-skill"].into_iter().map(|name| psa::api::ConfigId {
                    config_group: project.clone(),
                    name: name.into(),
                }).collect(),
            },
        }]).await.unwrap();
        let agent = harness
            .client
            .create_agent(
                session.id,
                &psa::api::CreateAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    name: "remote reader".into(),
                    config_id,
                    model: None,
                },
            )
            .await
            .unwrap();
        while harness.client.get_session(session.id).await.unwrap().busy {
            tokio::task::yield_now().await;
        }
        let page = harness
            .client
            .list_events(session.id, Some(agent.id), &psa::api::EventsQuery::default())
            .await
            .unwrap();
        let events: Vec<_> = page.events.iter().map(|event| &event.event.kind).collect();
        assert_eq!(events.len(), 6, "unexpected startup events: {events:#?}");
        assert!(matches!(events[0], EventType::NewContext));
        assert!(matches!(events[1], EventType::Model(_)));
        assert!(matches!(events[2], EventType::Message(message) if message.message_type == MessageType::System));
        assert!(matches!(events[3], EventType::ToolDefinition(_)));
        let EventType::Message(cwd_message) = events[4] else {
            panic!("cwd must follow tool definitions");
        };
        assert_eq!(cwd_message.message_type, MessageType::System);
        let escaped_cwd = cwd.to_str().unwrap().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        assert_eq!(
            serde_json::to_value(&cwd_message.content).unwrap(),
            serde_json::to_value(psa_server::prompts::text(format!("<cwd>\n{escaped_cwd}\n</cwd>"))).unwrap()
        );
        let catalogue = events[5..].iter().find_map(|event| match event {
            EventType::Message(message) if message.message_type == MessageType::System => Some(message),
            _ => None,
        }).expect("skills must emit a system catalogue after cwd");
        let content = serde_json::to_value(&catalogue.content).unwrap();
        let text: String = content.as_array().unwrap().iter().filter_map(|block| block["Text"]["text"].as_str()).collect();
        assert!(text.contains("<skills>"));
        assert!(text.contains("<available_skills>"));
        assert!(text.contains("review&lt;&amp;&gt;"));
        assert!(text.contains("Review &lt;code&gt; &amp; tests"));
        assert!(text.contains(&format!("{escaped_cwd}/.agents/skills/review/SKILL.md")));
        assert!(text.contains(&format!("{escaped_cwd}/.agents/skills/direct/SKILL.md")));
        // Named configuration references resolve in deterministic name order.
        assert!(text.find("<name>direct</name>").unwrap() < text.find("review&lt;&amp;&gt;").unwrap());
        assert!(!text.contains("hidden-skill"));
        assert!(!text.contains("SECRET"), "catalogue must not load skill bodies into context");
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn files_process_streams_signals_and_bundled_path() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let handle = harness.handle().await;
        let directory = harness.dir.path().join("files/nested");
        handle.create_dir(&directory).await.unwrap();
        let path = directory.join("binary");
        let bytes: Vec<u8> = (0..512_000).map(|i| (i % 251) as u8).collect();
        let call = handle.write_file(&path, false).await.unwrap();
        call.sender.write(0, &bytes).await.unwrap();
        call.sender.close_stdin().await.unwrap();
        collect(call).await;
        assert_eq!(
            collect(handle.read_file(&path).await.unwrap()).await.0,
            bytes
        );
        assert_eq!(handle.list_dir(&directory).await.unwrap()[0].name, "binary");
        let call = handle
            .spawn(
                Command::new("/bin/sh", &directory)
                    .arg("-c")
                    .arg("cat; printf error >&2; exit 7"),
            )
            .await
            .unwrap();
        let sender = call.sender.clone();
        let (_, (stdout, stderr, status)) = tokio::join!(
            async {
                sender.write(0, &bytes).await.unwrap();
                sender.close_stdin().await.unwrap();
            },
            collect(call)
        );
        assert_eq!(stdout, bytes);
        assert_eq!(stderr, b"error");
        assert_eq!(status.unwrap().code, Some(7));
        for program in ["rg", "fd"] {
            let mut command = Command::new(program, &directory).arg("--version");
            command.env_clear = true;
            let (stdout, _, status) = collect(handle.spawn(command).await.unwrap()).await;
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .starts_with(if program == "rg" { "ripgrep " } else { "fd " })
            );
            assert_eq!(status.unwrap().code, Some(0));
        }
        let mut call = handle
            .spawn(Command::new("sleep", &directory).arg("60"))
            .await
            .unwrap();
        assert!(matches!(
            call.next_event().await.unwrap(),
            Event::Started { .. }
        ));
        // Fill stdin against a child that does not read it. Signals must still work.
        let sender = call.sender.clone();
        let writer = tokio::spawn(async move { sender.write(0, &vec![0; 512_000]).await });
        call.sender.signal(15).await.unwrap();
        let (_, _, status) = collect(call).await;
        assert_eq!(status.unwrap().signal, Some(15));
        let _ = writer.await;
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disconnect_and_handle_drop_kill_owned_processes() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        for disconnect in [false, true] {
            let handle = harness.handle().await;
            let mut call = handle
                .spawn(Command::new("sleep", harness.dir.path()).arg("60"))
                .await
                .unwrap();
            let Event::Started { pid: Some(pid) } = call.next_event().await.unwrap() else {
                panic!("no pid")
            };
            if disconnect {
                harness.registry.disconnect(harness.id).await.unwrap();
                assert!(call.next_event().await.is_err());
            } else {
                drop(call);
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("child survived cancellation");
            if disconnect {
                harness.wait_active().await;
            }
        }
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sync_directory_publishes_only_after_eof_and_cleans_cancellation() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let handle = harness.handle().await;
        let source = harness.dir.path().join("sync-source");
        tokio::fs::create_dir(&source).await.unwrap();
        tokio::fs::write(source.join("new"), b"new").await.unwrap();
        let archive = collect(handle.download_dir(&source).await.unwrap()).await.0;
        let destination = harness.dir.path().join("sync-target");
        handle.create_dir(&destination).await.unwrap();
        tokio::fs::write(destination.join("old"), b"old")
            .await
            .unwrap();

        let call = handle.sync_dir(&destination).await.unwrap();
        call.sender.write(0, &archive).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            tokio::fs::read(destination.join("old")).await.unwrap(),
            b"old"
        );
        assert!(!destination.join("new").exists());
        call.sender.close_stdin().await.unwrap();
        collect(call).await;
        assert!(!destination.join("old").exists());
        assert_eq!(
            tokio::fs::read(destination.join("new")).await.unwrap(),
            b"new"
        );

        let call = handle.sync_dir(&destination).await.unwrap();
        call.sender.write(0, &[0; 256]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if std::fs::read_dir(harness.dir.path()).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".psa-upload-")
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(call);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !std::fs::read_dir(harness.dir.path()).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".psa-upload-")
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read(destination.join("new")).await.unwrap(),
            b"new"
        );

        let missing = harness.dir.path().join("sync-created");
        let call = handle.sync_dir(&missing).await.unwrap();
        call.sender.write(0, &archive).await.unwrap();
        call.sender.close_stdin().await.unwrap();
        collect(call).await;
        assert_eq!(tokio::fs::read(missing.join("new")).await.unwrap(), b"new");
        harness.stop().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn archive_roundtrip_rejects_links_and_cleans_interrupted_uploads() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let handle = harness.handle().await;
        let source = harness.dir.path().join("source");
        tokio::fs::create_dir_all(source.join("nested"))
            .await
            .unwrap();
        tokio::fs::write(source.join("nested/file"), b"archive contents")
            .await
            .unwrap();
        tokio::fs::set_permissions(
            source.join("nested/file"),
            std::os::unix::fs::PermissionsExt::from_mode(0o751),
        )
        .await
        .unwrap();
        let bytes = collect(handle.download_dir(&source).await.unwrap()).await.0;
        let destination = harness.dir.path().join("destination");
        let call = handle.upload_dir(&destination).await.unwrap();
        call.sender.write(0, &bytes).await.unwrap();
        call.sender.close_stdin().await.unwrap();
        collect(call).await;
        assert_eq!(
            tokio::fs::read(destination.join("nested/file"))
                .await
                .unwrap(),
            b"archive contents"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(destination.join("nested/file"))
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("/tmp/escape").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "link", std::io::empty())
            .unwrap();
        let bytes = archive.into_inner().unwrap();
        let rejected = harness.dir.path().join("rejected");
        let mut call = handle.upload_dir(&rejected).await.unwrap();
        call.sender.write(0, &bytes).await.unwrap();
        call.sender.close_stdin().await.unwrap();
        assert!(call.next_event().await.is_err());
        assert!(!rejected.exists());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..9].copy_from_slice(b"../escape");
        header.set_cksum();
        let mut archive = tar::Builder::new(Vec::new());
        archive.append(&header, std::io::empty()).unwrap();
        let traversal = archive.into_inner().unwrap();
        let mut call = handle
            .upload_dir(harness.dir.path().join("traversal"))
            .await
            .unwrap();
        call.sender.write(0, &traversal).await.unwrap();
        call.sender.close_stdin().await.unwrap();
        assert!(call.next_event().await.is_err());
        assert!(!harness.dir.path().join("escape").exists());
        let incomplete = harness.dir.path().join("incomplete");
        let call = handle.upload_dir(&incomplete).await.unwrap();
        call.sender.write(0, &[0; 256]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !std::fs::read_dir(harness.dir.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".psa-upload-")
            }) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(call);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!incomplete.exists());
        assert!(
            !std::fs::read_dir(harness.dir.path())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".psa-upload-"))
        );
        harness.stop().await;
    })
    .await
    .unwrap();
}
