use psa_api::{
    Client, ProbeId,
    probe_transport::{Call, ProbeError},
    probes::{Command, Event, ExitStatus, Uuid},
};
use psa_probe::config::StartOptions;
use psa_server::{
    db::Database,
    probes::{ProbeHandle, ProbeRegistry},
    providers::ProviderRegistry,
};
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

struct Harness {
    dir: tempfile::TempDir,
    client: Client,
    registry: ProbeRegistry,
    id: ProbeId,
    metadata: psa_api::ProbeMetadata,
    base: String,
    probe_stop: oneshot::Sender<()>,
    probe: tokio::task::JoinHandle<Result<(), psa_probe::ProbeError>>,
    server_stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), psa_server::server::ServerError>>,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("server.sqlite3"))
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let registry = ProbeRegistry::start("test-probe-key".into());
        let (server_stop, stopped) = oneshot::channel();
        let server = tokio::spawn(psa_server::server::serve_with_probe_registry(
            listener,
            database,
            ProviderRegistry::default(),
            registry.clone(),
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
        let config = StartOptions {
            data_dir: Some(probe_dir),
            server: Some(base.clone()),
            label: Some("integration".into()),
            ..Default::default()
        }
        .resolve()
        .await
        .unwrap();
        let id = config.metadata.id;
        let metadata = config.metadata.clone();
        let (probe_stop, stopped) = oneshot::channel();
        let probe = tokio::spawn(psa_probe::run(config, async {
            let _ = stopped.await;
        }));
        let harness = Self {
            client: Client::new(&base).unwrap(),
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
            reqwest::Url::parse(&psa_api::client::parse_probe_server_url(&self.base).unwrap())
                .unwrap();
        let mut url = url;
        url.query_pairs_mut()
            .append_pair("version", &version.to_string())
            .append_pair("id", &self.id.to_string())
            .append_pair("hostname", &self.metadata.hostname)
            .append_pair("os", "linux")
            .append_pair("architecture", "x86_64")
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
            ("bad-key", 1, 401),
            ("test-probe-key", 2, 400),
            ("test-probe-key", 1, 409),
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
            .create_session(&psa_api::CreateSession {
                name: "remote".into(),
                working_directory: "/not/on/server".into(),
                probe_id: harness.id,
                model_config: serde_json::from_value(serde_json::json!({
                    "model": { "api_name": "test-model" },
                    "provider": "Openrouter",
                    "effort": "Medium"
                }))
                .unwrap(),
            })
            .await
            .unwrap();
        let fork = harness
            .client
            .fork_session(
                session.id,
                &psa_api::ForkSession {
                    point: psa_api::ForkPoint::Head,
                    metadata: Default::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(fork.session.probe_id, harness.id);
        assert!(
            harness
                .client
                .create_session(&psa_api::CreateSession {
                    name: "bad".into(),
                    working_directory: "/tmp".into(),
                    probe_id: ProbeId(Uuid::new_v4()),
                    model_config: session.model_config.clone(),
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
async fn startup_reads_remote_skill_metadata_in_configured_order() {
    use psa_api::events::{EventType, MessageType};

    tokio::time::timeout(Duration::from_secs(30), async {
        let harness = Harness::start().await;
        let handle = harness.handle().await;
        let cwd = harness.dir.path().join("remote<&>");
        handle.create_dir(cwd.join("skills/review")).await.unwrap();
        for (path, contents) in [
            (
                "skills/review/SKILL.md",
                "---\nname: 'review<&>'\ndescription: 'Review <code> & tests'\n---\nSECRET DIRECTORY SKILL BODY\n",
            ),
            (
                "skills/direct.md",
                "---\nname: direct\ndescription: A direct file\n---\nSECRET DIRECT SKILL BODY\n",
            ),
            (
                "skills/hidden.md",
                "---\nname: hidden-skill\ndescription: hidden description\ndisable-model-invocation: true\n---\nSECRET HIDDEN SKILL BODY\n",
            ),
        ] {
            let call = handle.write_file(cwd.join(path), false).await.unwrap();
            call.sender.write(0, contents.as_bytes()).await.unwrap();
            call.sender.close_stdin().await.unwrap();
            collect(call).await;
        }
        let definition = harness
            .client
            .create_agent_definition(&psa_api::CreateAgentDefinition {
                name: "remote skills".into(),
                tools: vec![psa_api::ToolId::Noop],
                hooks: psa_api::HookIds {
                    agent_create: vec![
                        psa_api::HookId::DefaultStartup,
                        psa_api::HookId::ToolDefinitions,
                        psa_api::HookId::Cwd,
                        psa_api::HookId::Skills,
                    ],
                    ..Default::default()
                },
                skills: vec![
                    "skills/review".into(),
                    "skills/direct.md".into(),
                    "skills/hidden.md".into(),
                    "skills/missing".into(),
                ],
                ..Default::default()
            })
            .await
            .unwrap();
        let session = harness
            .client
            .create_session(&psa_api::CreateSession {
                name: "skills".into(),
                working_directory: cwd.clone(),
                probe_id: harness.id,
                model_config: serde_json::from_value(serde_json::json!({
                    "model": { "api_name": "test-model" },
                    "provider": "Openrouter",
                    "effort": "Medium"
                }))
                .unwrap(),
            })
            .await
            .unwrap();
        let agent = harness
            .client
            .create_agent(
                session.id,
                &psa_api::CreateAgent {
                    subagent_parent_id: None,
                    interactive: false,
                    name: "remote reader".into(),
                    definition_id: definition.id,
                },
            )
            .await
            .unwrap();
        while harness.client.get_session(session.id).await.unwrap().busy {
            tokio::task::yield_now().await;
        }
        let page = harness
            .client
            .events(session.id, Some(agent.id), &psa_api::EventsQuery::default())
            .await
            .unwrap();
        let events: Vec<_> = page.events.iter().map(|event| &event.event.kind).collect();
        assert_eq!(events.len(), 7, "unexpected startup events: {events:#?}");
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
        assert!(events[5..].iter().any(|event| matches!(event, EventType::Log(log) if !log.error && log.message.contains("missing"))));
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
        assert!(text.contains(&format!("{escaped_cwd}/skills/review/SKILL.md")));
        assert!(text.contains(&format!("{escaped_cwd}/skills/direct.md")));
        assert!(text.find("review&lt;&amp;&gt;").unwrap() < text.find("<name>direct</name>").unwrap());
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
