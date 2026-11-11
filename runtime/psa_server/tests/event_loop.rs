//! Real HTTP/client/SSE tests against an in-process fake inference endpoint.

#[path = "event_loop/compaction.rs"]
mod compaction_features;
#[path = "event_loop/components.rs"]
mod component_features;
#[path = "support/fake_probe.rs"]
mod fake_probe;
#[path = "event_loop/model_choices.rs"]
mod model_choices;
#[path = "event_loop/results.rs"]
mod result_features;
mod support;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::StreamExt;
use llm_api::{
    api::{self, OpenAiCompletions, OpenAiCompletionsConfig, Provider},
    providers::{Effort, Model, ModelConfig, ProviderType},
};
use psa_api::{self as wire, AgentRequest, AgentState, MessageDelivery, StreamMessage, events::*};
use psa_server::{
    agents::{
        NewAgent, ToolDefinitionExt, agent_manager::AgentManagerState, publication::Publication,
        session_manager::Runtime,
    },
    components::tools::ToolId,
    db::{Database, DbError},
    probes::ProbeRegistry,
    providers::ProviderRegistry,
    sessions::{ForkPoint, NewSession, SessionFork, SessionUpdate},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
};

fn model() -> ModelConfig {
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::Medium,
    }
}

fn config_id(name: &str) -> wire::ConfigId {
    wire::ConfigId {
        config_group: wire::ConfigGroup::Custom("test".into()),
        name: name.into(),
    }
}

fn agent_config(name: &str) -> wire::AgentConfig {
    wire::AgentConfig {
        config_id: config_id(name),
        description: String::new(),
        selectable: true,
        bundles: vec![],
        hooks: Default::default(),
        models: vec![wire::ModelSelection::Global],
        default: true,
        subagents: Default::default(),
        skills: Default::default(),
    }
}

#[derive(Clone, Copy)]
enum Script {
    Text,
    Tools,
    TextTools,
    TokenLimit,
    Calls(&'static [(&'static str, &'static str, &'static str)]),
    Wait,
}

#[derive(Clone)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<Script>>>,
    requests: mpsc::UnboundedSender<Value>,
    stop: watch::Receiver<bool>,
}

async fn inference(State(mut fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let script = fake
        .scripts
        .lock()
        .await
        .pop_front()
        .unwrap_or(Script::Text);
    fake.requests.send(body).unwrap();
    let mut delta = match script {
        Script::Tools | Script::TextTools => json!({"role":"assistant", "tool_calls":[
            {"index":0,"id":"call-ok","type":"function","function":{"name":"noop","arguments":"{}"}},
            {"index":1,"id":"call-bad","type":"function","function":{"name":"noop","arguments":"{bad json"}}
        ]}),
        Script::Calls(calls) => {
            json!({"role":"assistant", "tool_calls": calls.iter().enumerate().map(|(index, (id, name, arguments))| {
            json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":arguments}})
        }).collect::<Vec<_>>() })
        }
        _ => json!({"role":"assistant", "content":"hello"}),
    };
    if matches!(script, Script::TextTools) {
        delta["content"] = json!("intermediate message");
    }
    let first = format!(
        "data: {}\n\n",
        json!({"id":"fake-response","choices":[{"index":0,"delta":delta,"finish_reason":null}]})
    );
    let finish = if matches!(script, Script::TokenLimit) {
        "length"
    } else if matches!(script, Script::Tools | Script::TextTools | Script::Calls(_)) {
        "tool_calls"
    } else {
        "stop"
    };
    let last = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})
    );
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(first);
        if matches!(script, Script::Wait) { let _ = fake.stop.changed().await; }
        else { yield Ok(last); }
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Database,
    client: wire::Client,
    address: std::net::SocketAddr,
    requests: mpsc::UnboundedReceiver<Value>,
    scripts: Arc<Mutex<VecDeque<Script>>>,
    stop: oneshot::Sender<()>,
    server: JoinHandle<Result<(), psa_server::server::ServerError>>,
    fake_stop: watch::Sender<bool>,
    fake_server: JoinHandle<()>,
    probes: ProbeRegistry,
    probe: Option<fake_probe::FakeProbe>,
}

impl Harness {
    async fn start(scripts: Vec<Script>) -> Self {
        Self::start_with_probe(scripts, true).await
    }

    async fn start_with_probe(scripts: Vec<Script>, connect_probe: bool) -> Self {
        Self::start_with_files(scripts, connect_probe, fake_probe::FakeFiles::default()).await
    }

    async fn start_with_files(
        scripts: Vec<Script>,
        connect_probe: bool,
        files: fake_probe::FakeFiles,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.sqlite3"))
            .await
            .unwrap();
        support::register_probe(&db).await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = listener.local_addr().unwrap();
        let (requests, received) = mpsc::unbounded_channel();
        let (fake_stop, stopped) = watch::channel(false);
        let scripts = Arc::new(Mutex::new(scripts.into()));
        let app = Router::new()
            .route("/inference", post(inference))
            .route(
                "/api/models/providers/{provider}",
                axum::routing::get(|| async { Json(json!([])) }),
            )
            .with_state(Fake {
                scripts: scripts.clone(),
                requests,
                stop: stopped.clone(),
            });
        let fake_server = tokio::spawn(async move {
            let mut stopped = stopped;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await
                .unwrap();
        });

        let provider = OpenAiCompletions::new(
            format!("http://{fake_address}/inference"),
            "fake-key",
            ProviderType::Openrouter,
            OpenAiCompletionsConfig::default(),
        )
        .unwrap();
        let providers = ProviderRegistry::new(vec![Provider::Completions(provider)])
            .with_catalog_source(
                psa_server::models::CatalogSource::new(&format!("http://{fake_address}")).unwrap(),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let probes = ProbeRegistry::start(fake_probe::PROBE_KEY.into());
        let server = tokio::spawn(psa_server::server::serve_with_configuration(
            listener,
            db.clone(),
            providers,
            probes.clone(),
            psa_server::config_registry::ConfigRegistry::new(dir.path().join("config")),
            psa_server::config::default_model_config(),
            "test-client-key".into(),
            async {
                let _ = stopped.await;
            },
        ));

        let probe = if connect_probe {
            Some(fake_probe::FakeProbe::connect_with_files(address, &probes, files).await)
        } else {
            None
        };

        let client = wire::Client::new(&format!("http://{address}"), "test-client-key").unwrap();
        let mut default = agent_config("default");
        default.bundles = vec![psa_api::BundleConfig::Noop];
        default.hooks = wire::HookConfigs {
            agent_create: vec![
                wire::HookConfig::DefaultStartup,
                wire::HookConfig::ToolDefinitions,
            ],
            session_update: vec![wire::HookConfig::SessionModelUpdate],
            after_request: vec![wire::HookConfig::StopWithoutTools],
            ..Default::default()
        };
        client.upsert_agent_configs(&[default]).await.unwrap();

        Self {
            _dir: dir,
            db,
            client,
            address,
            requests: received,
            scripts,
            stop,
            server,
            fake_stop,
            fake_server,
            probes,
            probe,
        }
    }

    async fn upsert_config(&self, config: wire::AgentConfig) -> wire::AgentConfig {
        let statuses = self
            .client
            .upsert_agent_configs(std::slice::from_ref(&config))
            .await
            .unwrap();
        assert!(statuses[0].status.missing_components.is_empty());
        config
    }

    async fn create_agent(&self) -> (wire::Session, wire::Agent) {
        let session = self
            .client
            .create_session(&wire::CreateSession {
                model_config: Some(model()),
                probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
                name: "session".into(),
                working_directory: "/work".into(),
            })
            .await
            .unwrap();
        let agent = self
            .client
            .create_agent(
                session.id,
                &wire::NewConfiguredAgent {
                    model: None,
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    name: "agent".into(),
                    config_id: config_id("default"),
                },
            )
            .await
            .unwrap();
        (session, agent)
    }

    async fn wait_stopped(&self, session: wire::SessionId) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.client.get_session(session).await.unwrap().busy {
                // Session reads share the database cursor with worker writes.
                // Leave time for progress instead of immediately polling again.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn disconnect_probe(&mut self) {
        self.probe.take().unwrap().disconnect(&self.probes).await;
    }

    async fn close(self) {
        self.stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.fake_stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), self.fake_server)
            .await
            .unwrap()
            .unwrap();
    }
}

fn message(delivery: MessageDelivery) -> AgentRequest {
    AgentRequest::Message {
        delivery,
        content: psa_server::prompts::text("test input"),
    }
}

#[tokio::test]
async fn interactive_and_hidden_metadata_survive_http_create_get_list_and_fork() {
    let harness = Harness::start(vec![]).await;
    let (session, ordinary) = harness.create_agent().await;
    assert!(!ordinary.interactive);
    assert!(!ordinary.hidden);
    let interactive = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                model: None,
                subagent_parent_id: None,
                name: "interactive".into(),
                config_id: config_id("default"),
                interactive: true,
                hidden: true,
            },
        )
        .await
        .unwrap();
    assert!(interactive.interactive);
    assert!(interactive.hidden);
    harness.wait_stopped(session.id).await;

    let agents = harness.client.list_agents(session.id).await.unwrap();
    assert_eq!(agents.len(), 2);
    for source in [&ordinary, &interactive] {
        let loaded = harness
            .client
            .get_agent(session.id, source.id)
            .await
            .unwrap();
        assert_eq!(loaded.interactive, source.interactive);
        assert_eq!(loaded.hidden, source.hidden);
        let listed = agents.iter().find(|agent| agent.id == source.id).unwrap();
        assert_eq!(listed.interactive, source.interactive);
        assert_eq!(listed.hidden, source.hidden);
    }

    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(fork.session.id).await;
    assert_eq!(fork.agents.len(), 2);
    let children = harness.client.list_agents(fork.session.id).await.unwrap();
    assert_eq!(children.len(), 2);
    for source in [&ordinary, &interactive] {
        let child = fork
            .agents
            .iter()
            .find(|agent| agent.parent_id == Some(source.id))
            .unwrap();
        assert_eq!(child.interactive, source.interactive);
        assert_eq!(child.hidden, source.hidden);
        assert_eq!(child.definition_id, source.definition_id);
        assert_eq!(child.parent_id, Some(source.id));
        let loaded = harness
            .client
            .get_agent(fork.session.id, child.id)
            .await
            .unwrap();
        assert_eq!(loaded.interactive, source.interactive);
        assert_eq!(loaded.hidden, source.hidden);
        let listed = children.iter().find(|agent| agent.id == child.id).unwrap();
        assert_eq!(listed.interactive, source.interactive);
        assert_eq!(listed.hidden, source.hidden);
    }
    harness.close().await;
}

#[tokio::test]
async fn session_project_selection_persists_and_rediscovery_tracks_cwd_and_forks() {
    let harness = Harness::start_with_files(
        vec![],
        true,
        fake_probe::FakeFiles::default()
            .directory("/work/.agents")
            .directory("/work/project/.agents")
            .directory("/else/.agents"),
    )
    .await;
    let session = harness
        .client
        .create_session(&wire::CreateSession {
            name: "project".into(),
            working_directory: "/work/project/nested".into(),
            probe_id: support::probe_metadata().id,
            model_config: Some(model()),
        })
        .await
        .unwrap();
    let expected = Some(wire::ConfigGroup::Project {
        path: "/work/project/.agents".into(),
        probe_id: support::probe_metadata().id,
    });
    let mut cursor = harness.db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_session_project(session.id).await.unwrap(),
        expected
    );
    cursor.rollback().await.unwrap();

    let inherited = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let moved = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: wire::UpdateSession {
                    working_directory: Some("/else/child".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    let mut cursor = harness.db.cursor().await.unwrap();
    assert_eq!(
        cursor
            .get_session_project(inherited.session.id)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        cursor.get_session_project(moved.session.id).await.unwrap(),
        Some(wire::ConfigGroup::Project {
            path: "/else/.agents".into(),
            probe_id: support::probe_metadata().id,
        })
    );
    assert_eq!(
        cursor.get_session_project(session.id).await.unwrap(),
        expected
    );
    cursor.rollback().await.unwrap();

    harness
        .client
        .update_session(
            session.id,
            &wire::UpdateSession {
                working_directory: Some("/outside".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut cursor = harness.db.cursor().await.unwrap();
    assert_eq!(cursor.get_session_project(session.id).await.unwrap(), None);
    assert_eq!(
        cursor
            .get_session_project(inherited.session.id)
            .await
            .unwrap(),
        expected
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn config_routes_preserve_snapshots_and_explicit_hook_configuration() {
    let harness = Harness::start(vec![]).await;
    let mut request = agent_config("custom");
    request.description = "An HTTP snapshot fixture".into();
    request.selectable = true;
    request.bundles = vec![psa_api::BundleConfig::Noop];
    request.hooks = wire::HookConfigs {
        agent_create: vec![
            wire::HookConfig::Noop,
            wire::HookConfig::ExtraSystemPrompt {
                prompt: "Review carefully.".into(),
            },
        ],
        session_update: vec![wire::HookConfig::Noop],
        before_request: vec![wire::HookConfig::Noop],
        after_request: vec![wire::HookConfig::Noop],
    };
    harness.upsert_config(request.clone()).await;
    let loaded = harness
        .client
        .get_agent_config(&request.config_id)
        .await
        .unwrap();
    assert!(loaded.status.missing_components.is_empty());
    assert_eq!(
        serde_json::to_value(&loaded.config).unwrap(),
        serde_json::to_value(&request).unwrap()
    );

    let (session, _) = harness.create_agent().await;
    let original = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                config_id: request.config_id.clone(),
                name: "pinned".into(),
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                model: None,
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    let snapshot = harness
        .client
        .get_resolved_agent_config(original.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&snapshot.config).unwrap(),
        serde_json::to_value(&request).unwrap()
    );
    let events = harness
        .client
        .events(session.id, Some(original.id), &wire::EventsQuery::default())
        .await
        .unwrap();
    assert_eq!(
        events.events.len(),
        1,
        "explicit hooks must not acquire default startup hooks"
    );
    assert!(matches!(&events.events[0].event.kind,
        EventType::Message(message) if message.message_type == MessageType::System
            && matches!(&message.content[..], [api::ContentBlock::Text(text)] if text.text == "Review carefully.")
    ));

    let mut replacement = request.clone();
    replacement.hooks.agent_create = vec![
        wire::HookConfig::DefaultStartup,
        wire::HookConfig::ExtraSystemPrompt {
            prompt: "Updated prompt.".into(),
        },
        wire::HookConfig::ToolDefinitions,
    ];
    harness.upsert_config(replacement.clone()).await;
    let updated = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                config_id: request.config_id.clone(),
                name: "updated".into(),
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                model: None,
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_ne!(updated.definition_id, original.definition_id);
    assert_eq!(
        harness
            .client
            .get_agent(session.id, original.id)
            .await
            .unwrap()
            .definition_id,
        original.definition_id
    );
    let retained = harness
        .client
        .get_resolved_agent_config(original.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&retained).unwrap(),
        serde_json::to_value(&snapshot).unwrap()
    );
    let updated_snapshot = harness
        .client
        .get_resolved_agent_config(updated.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_snapshot.config.hooks.agent_create,
        replacement.hooks.agent_create
    );
    assert_eq!(updated_snapshot.definition.hooks, replacement.hooks);
    let configs = harness
        .client
        .refresh_and_list_agent_configs(Some(session.id))
        .await
        .unwrap();
    assert_eq!(
        configs
            .iter()
            .filter(|entry| entry.config.config_id == request.config_id)
            .count(),
        1
    );
    assert_eq!(
        harness
            .client
            .get_agent_config(&request.config_id)
            .await
            .unwrap()
            .config
            .hooks
            .agent_create,
        replacement.hooks.agent_create
    );
    harness.close().await;
}

#[tokio::test]
async fn config_batches_cycles_and_dependency_changes_pin_complete_graphs() {
    let harness = Harness::start(vec![]).await;
    let mut first = agent_config("cycle-first");
    let mut second = agent_config("cycle-second");
    first.subagents.items = vec![second.config_id.clone()];
    second.subagents.items = vec![first.config_id.clone()];
    let statuses = harness
        .client
        .upsert_agent_configs(&[first.clone(), second.clone()])
        .await
        .unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(
        statuses
            .iter()
            .all(|entry| entry.status.missing_components.is_empty())
    );
    let (session, _) = harness.create_agent().await;
    let create = |name: &str| wire::NewConfiguredAgent {
        config_id: first.config_id.clone(),
        name: name.into(),
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        model: None,
    };
    let original = harness
        .client
        .create_agent(session.id, &create("original"))
        .await
        .unwrap();
    let same = harness
        .client
        .create_agent(session.id, &create("same"))
        .await
        .unwrap();
    assert_eq!(
        same.definition_id, original.definition_id,
        "unchanged graphs reuse snapshots"
    );
    let snapshot = harness
        .client
        .get_resolved_agent_config(original.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.references.subagents.len(), 1);
    let child = harness
        .client
        .get_resolved_agent_config(snapshot.references.subagents[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.config.config_id, second.config_id);
    assert_eq!(child.references.subagents, vec![original.definition_id]);

    second
        .hooks
        .agent_create
        .push(wire::HookConfig::ExtraSystemPrompt {
            prompt: "Changed dependency".into(),
        });
    harness.upsert_config(second.clone()).await;
    let changed = harness
        .client
        .create_agent(session.id, &create("changed"))
        .await
        .unwrap();
    assert_ne!(changed.definition_id, original.definition_id);
    let retained = harness
        .client
        .get_resolved_agent_config(original.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.references.subagents, snapshot.references.subagents);
    let retained_child = harness
        .client
        .get_resolved_agent_config(child.definition.id)
        .await
        .unwrap()
        .unwrap();
    assert!(retained_child.config.hooks.agent_create.is_empty());

    let mut invalid = agent_config("invalid-batch");
    invalid.models.clear();
    let never_written = agent_config("never-written");
    assert!(
        harness
            .client
            .upsert_agent_configs(&[never_written.clone(), invalid])
            .await
            .is_err()
    );
    assert!(
        !harness
            .client
            .refresh_and_list_agent_configs(None)
            .await
            .unwrap()
            .iter()
            .any(|entry| entry.config.config_id == never_written.config_id)
    );
    harness.wait_stopped(session.id).await;
    harness.close().await;
}

#[tokio::test]
async fn group_membership_changes_only_advance_new_agent_snapshots() {
    let harness = Harness::start(vec![]).await;
    let group = wire::ConfigGroup::Custom("dynamic-members".into());
    let mut parent = agent_config("group-parent");
    parent.subagents.groups = vec![group.clone()];
    harness.upsert_config(parent.clone()).await;
    let (session, _) = harness.create_agent().await;
    let request = |name: &str| wire::NewConfiguredAgent {
        config_id: parent.config_id.clone(),
        name: name.into(),
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        model: None,
    };
    let original = harness
        .client
        .create_agent(session.id, &request("before"))
        .await
        .unwrap();
    let before = harness
        .client
        .get_resolved_agent_config(original.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert!(before.references.subagents.is_empty());

    let mut member = agent_config("member");
    member.config_id.config_group = group;
    harness.upsert_config(member.clone()).await;
    let updated = harness
        .client
        .create_agent(session.id, &request("after"))
        .await
        .unwrap();
    assert_ne!(updated.definition_id, original.definition_id);
    let after = harness
        .client
        .get_resolved_agent_config(updated.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.references.subagents.len(), 1);
    let selected = harness
        .client
        .get_resolved_agent_config(after.references.subagents[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.config.config_id, member.config_id);
    assert!(
        harness
            .client
            .get_resolved_agent_config(original.definition_id)
            .await
            .unwrap()
            .unwrap()
            .references
            .subagents
            .is_empty()
    );
    harness.wait_stopped(session.id).await;
    harness.close().await;
}

#[tokio::test]
async fn missing_explicit_dependencies_warn_and_resolve_again_when_registered() {
    let harness = Harness::start(vec![]).await;
    let mut parent = agent_config("missing-parent");
    let child = agent_config("missing-child");
    parent.subagents.items = vec![child.config_id.clone()];
    let statuses = harness
        .client
        .upsert_agent_configs(&[parent.clone()])
        .await
        .unwrap();
    assert_eq!(
        statuses[0].status.missing_components,
        vec![wire::MissingComponent::Agent {
            config_id: child.config_id.clone()
        }]
    );
    let (session, _) = harness.create_agent().await;
    let request = wire::NewConfiguredAgent {
        config_id: parent.config_id.clone(),
        name: "parent".into(),
        subagent_parent_id: None,
        interactive: false,
        hidden: false,
        model: None,
    };
    let incomplete = harness
        .client
        .create_agent(session.id, &request)
        .await
        .unwrap();
    let incomplete_snapshot = harness
        .client
        .get_resolved_agent_config(incomplete.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert!(incomplete_snapshot.references.subagents.is_empty());
    assert_eq!(
        incomplete_snapshot.config.subagents.items,
        parent.subagents.items
    );
    harness.upsert_config(child).await;
    assert!(
        harness
            .client
            .get_agent_config(&parent.config_id)
            .await
            .unwrap()
            .status
            .missing_components
            .is_empty()
    );
    let agent = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                name: "restored".into(),
                ..request
            },
        )
        .await
        .unwrap();
    assert_ne!(agent.definition_id, incomplete.definition_id);
    assert!(
        harness
            .client
            .get_resolved_agent_config(incomplete.definition_id)
            .await
            .unwrap()
            .unwrap()
            .references
            .subagents
            .is_empty()
    );
    let snapshot = harness
        .client
        .get_resolved_agent_config(agent.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.references.subagents.len(), 1);
    harness.wait_stopped(session.id).await;
    harness.close().await;
}

#[tokio::test]
async fn startup_tools_and_cwd_are_independent_ordered_offline_hooks() {
    let harness = Harness::start_with_probe(vec![], false).await;
    let session = harness
        .client
        .create_session(&wire::CreateSession {
            name: "offline hooks".into(),
            working_directory: "/work/<project>&".into(),
            probe_id: support::probe_metadata().id,
            model_config: Some(model()),
        })
        .await
        .unwrap();

    for (name, hooks) in [
        ("omitted", vec![]),
        ("startup", vec![wire::HookConfig::DefaultStartup]),
        ("tools", vec![wire::HookConfig::ToolDefinitions]),
        (
            "ordered",
            vec![
                wire::HookConfig::DefaultStartup,
                wire::HookConfig::Cwd,
                wire::HookConfig::ToolDefinitions,
            ],
        ),
    ] {
        let definition = harness
            .upsert_config(wire::AgentConfig {
                bundles: vec![psa_api::BundleConfig::Noop],
                hooks: wire::HookConfigs {
                    agent_create: hooks,
                    ..Default::default()
                },
                ..agent_config(name)
            })
            .await;
        let agent = harness
            .client
            .create_agent(
                session.id,
                &wire::NewConfiguredAgent {
                    model: None,
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    name: name.into(),
                    config_id: definition.config_id,
                },
            )
            .await
            .unwrap();
        harness.wait_stopped(session.id).await;
        let mut cursor = harness.db.cursor().await.unwrap();
        let events = cursor.load_all_events(agent.id).await.unwrap();
        cursor.rollback().await.unwrap();

        if name == "omitted" {
            assert!(
                events.is_empty(),
                "omitted hooks must not acquire startup or tools"
            );
            continue;
        }
        if name == "tools" {
            assert_eq!(events.len(), 1);
            assert!(
                matches!(&events[0].kind, EventType::ToolDefinition(tool) if tool.id == ToolId::Noop)
            );
            continue;
        }

        assert_eq!(events.len(), if name == "ordered" { 5 } else { 3 });
        assert!(matches!(events[0].kind, EventType::NewContext));
        assert!(matches!(&events[1].kind, EventType::Model(config) if config == &model()));
        let EventType::Message(base) = &events[2].kind else {
            panic!("startup must emit a separate base system message");
        };
        assert_eq!(base.message_type, MessageType::System);
        assert_eq!(
            serde_json::to_value(&base.content).unwrap(),
            serde_json::to_value(psa_server::prompts::text(
                "You are an expert coding assistant operating inside psa, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files."
            ))
            .unwrap()
        );
        if name == "ordered" {
            let EventType::Message(cwd) = &events[3].kind else {
                panic!("cwd must follow the base system message");
            };
            assert_eq!(cwd.message_type, MessageType::System);
            assert_eq!(
                serde_json::to_value(&cwd.content).unwrap(),
                serde_json::to_value(psa_server::prompts::text(
                    "<cwd>\n/work/&lt;project&gt;&amp;\n</cwd>"
                ))
                .unwrap()
            );
            assert!(matches!(events[4].kind, EventType::ToolDefinition(_)));
        }
    }
    assert!(harness.requests.is_empty());

    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: wire::UpdateSession {
                    working_directory: Some("/fork".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(fork.session.id).await;
    let ordered = fork
        .agents
        .iter()
        .find(|agent| agent.name == "ordered")
        .unwrap();
    let mut cursor = harness.db.cursor().await.unwrap();
    let events = cursor.load_all_events(ordered.id).await.unwrap();
    assert_eq!(
        events.len(),
        6,
        "forks inherit startup and tools, but refresh cwd"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventType::ToolDefinition(_)))
            .count(),
        1
    );
    let EventType::Message(cwd) = &events.last().unwrap().kind else {
        panic!("fork must append its own cwd");
    };
    assert_eq!(
        serde_json::to_value(&cwd.content).unwrap(),
        serde_json::to_value(psa_server::prompts::text("<cwd>\n/fork\n</cwd>")).unwrap()
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn offline_probe_interrupts_before_calling_the_provider() {
    let mut harness = Harness::start_with_probe(vec![], false).await;
    let (session, agent) = harness.create_agent().await;

    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Interrupted
    );
    assert!(harness.requests.try_recv().is_err());

    let mut cursor = harness.db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventType::StatusChange(change)
            if change.new_status == AgentState::Interrupted && change.reason.is_some()
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.kind, EventType::InferenceResultStart(_)))
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn disconnected_probe_interrupts_the_next_request_before_provider_call() {
    let mut harness = Harness::start(vec![Script::Text]).await;
    let (session, agent) = harness.create_agent().await;

    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );

    harness.disconnect_probe().await;
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    let agent = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(agent.state, AgentState::Interrupted);
    assert_eq!(agent.usage.total_tokens, Some(7));
    assert!(harness.requests.try_recv().is_err());

    let mut cursor = harness.db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::InferenceResultStart(_)))
            .count(),
        1
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn queued_requests_and_worker_retirement_do_not_lose_messages() {
    let mut harness = Harness::start(vec![]).await;
    let (session, agent) = harness.create_agent().await;
    // Repeatedly cross retirement boundaries, then race a burst of admissions.
    for _ in 0..4 {
        harness
            .client
            .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
            .await
            .unwrap();
        harness.requests.recv().await.unwrap();
        harness.wait_stopped(session.id).await;
    }
    let request = message(MessageDelivery::Queue);
    let replies = futures_util::future::join_all((0..12).map(|_| {
        harness
            .client
            .submit_request(session.id, agent.id, &request)
    }))
    .await;
    assert!(replies.iter().all(Result::is_ok));
    for _ in 0..12 {
        harness.requests.recv().await.unwrap();
    }
    harness.wait_stopped(session.id).await;
    let mut cursor = harness.db.cursor().await.unwrap();
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Human)).count(), 16);
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().usage.total_tokens,
        Some(16 * 7)
    );
    cursor.rollback().await.unwrap();
    harness.close().await;
}

#[tokio::test]
async fn busy_queries_track_all_workers_and_request_ids_ignore_generations() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = Harness::start(vec![Script::Wait, Script::Wait]).await;
        let (session, first) = harness.create_agent().await;
        let second = harness
            .client
            .create_agent(
                session.id,
                &wire::NewConfiguredAgent {
                    model: None,
                    subagent_parent_id: None,
                    interactive: false,
                    hidden: false,
                    name: "second".into(),
                    config_id: config_id("default"),
                },
            )
            .await
            .unwrap();
        harness.wait_stopped(session.id).await;
        assert!(!harness.client.get_session(session.id).await.unwrap().busy);

        for (index, agent) in [first.id, second.id].into_iter().enumerate() {
            let accepted = harness
                .client
                .submit_request(session.id, agent, &message(MessageDelivery::Queue))
                .await
                .unwrap();
            assert_eq!(
                accepted.request_id.rsplit('-').next().unwrap(),
                (index + 1).to_string()
            );
            harness.requests.recv().await.unwrap();
        }

        let updated = harness
            .client
            .update_session(
                session.id,
                &wire::UpdateSession {
                    name: Some("updated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "updated");
        assert!(updated.busy);
        assert!(harness.client.list_sessions(None).await.unwrap()[0].busy);

        harness
            .client
            .submit_request(session.id, first.id, &AgentRequest::Interrupt)
            .await
            .unwrap();
        while harness
            .client
            .get_agent(session.id, first.id)
            .await
            .unwrap()
            .state
            != AgentState::Interrupted
        {
            tokio::task::yield_now().await;
        }
        assert!(harness.client.get_session(session.id).await.unwrap().busy);

        harness
            .client
            .submit_request(session.id, second.id, &AgentRequest::Interrupt)
            .await
            .unwrap();
        harness.wait_stopped(session.id).await;
        assert!(!harness.client.list_sessions(None).await.unwrap()[0].busy);
        harness.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn interrupt_and_resume_restart_the_worker_without_replaying_the_request() {
    let mut harness = Harness::start(vec![Script::Wait, Script::Text]).await;
    let (session, agent) = harness.create_agent().await;
    assert!(matches!(
        harness
            .client
            .submit_request(session.id, agent.id, &message(MessageDelivery::Resume))
            .await,
        Err(wire::ClientError::Api { status: 409, .. })
    ));
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness
        .client
        .submit_request(session.id, agent.id, &AgentRequest::Interrupt)
        .await
        .unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Interrupted
    );
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Resume))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness.wait_stopped(session.id).await;
    assert_eq!(
        harness
            .client
            .get_agent(session.id, agent.id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
    harness.close().await;
}

#[tokio::test]
async fn http_tools_hooks_usage_streaming_and_reconnect() {
    let mut harness = Harness::start(vec![Script::Tools, Script::Text]).await;
    let (session, agent) = harness.create_agent().await;
    // Observe inference's busy interval, not the preceding creation hook's.
    harness.wait_stopped(session.id).await;
    let query = wire::EventsQuery {
        limit: 1,
        include_pending: true,
        ..Default::default()
    };
    let mut stream = harness
        .client
        .stream_events(session.id, None, &query, None)
        .await
        .unwrap();
    let mut durable = Vec::new();
    let mut resume_id = String::new();
    // A paginated initial replay must remain connected afterward.
    loop {
        let event = stream.next().await.unwrap().unwrap();
        match event.message {
            StreamMessage::Committed { event, .. } => durable.push(event),
            StreamMessage::Snapshot { .. } => {
                resume_id.clone_from(&event.id);
                break;
            }
            _ => {}
        }
    }
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    let first = harness.requests.recv().await.unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 2);
    assert_eq!(first["messages"][0]["role"], "system");
    assert_eq!(first["messages"][1]["role"], "user");
    let mut pending_seen = false;
    let mut promoted = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = stream.next().await.unwrap().unwrap();
            match event.message {
                StreamMessage::Upsert { .. } => pending_seen = true,
                StreamMessage::Committed { event, pending_id } => {
                    promoted |= pending_id.is_some();
                    durable.push(event);
                }
                StreamMessage::SessionUpdated { session: snapshot }
                    if snapshot.id == session.id && !snapshot.busy =>
                {
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(pending_seen && promoted);
    let second = harness.requests.recv().await.unwrap();
    assert!(
        second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool")
    );
    let agent_view = harness
        .client
        .get_agent(session.id, agent.id)
        .await
        .unwrap();
    assert_eq!(agent_view.state, AgentState::Idle);
    assert_eq!(agent_view.usage.total_tokens, Some(14));
    let page = harness
        .client
        .events(
            session.id,
            None,
            &wire::EventsQuery {
                limit: 10_000,
                include_pending: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(page.pending.is_empty());
    assert_eq!(durable.len(), page.events.len());
    assert!(
        durable
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
    let results: Vec<_> = page
        .events
        .iter()
        .filter_map(|event| match &event.event.kind {
            EventType::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    // Parallel invocations persist in completion order, not declaration order.
    assert!(
        !results
            .iter()
            .find(|result| result.call_id == "call-ok")
            .unwrap()
            .error
    );
    assert!(
        results
            .iter()
            .find(|result| result.call_id == "call-bad")
            .unwrap()
            .error
    );
    let mut reconnected = harness
        .client
        .stream_events(
            session.id,
            None,
            &wire::EventsQuery::default(),
            Some(&resume_id),
        )
        .await
        .unwrap();
    let mut replayed = 0;
    loop {
        match reconnected.next().await.unwrap().unwrap().message {
            StreamMessage::Committed { .. } => replayed += 1,
            StreamMessage::Snapshot { .. } => break,
            _ => {}
        }
    }
    assert_eq!(replayed, page.events.len() - 4); // initial NewContext + Model + System + ToolDefinition
    let other = harness
        .client
        .create_session(&wire::CreateSession {
            model_config: Some(model()),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "other".into(),
            working_directory: "/other".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        harness.client.get_agent(other.id, agent.id).await,
        Err(wire::ClientError::Api { status: 404, .. })
    ));
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(fork.session.id).await;
    assert_eq!(fork.agents[0].usage, agent_view.usage);
    // Leave SSE connections open: shutdown must close them, not hang draining.
    harness.close().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn shutdown_interrupts_inference_persists_partial_output_and_discards_queue() {
    let mut harness = Harness::start(vec![Script::Wait]).await;
    let (session, agent) = harness.create_agent().await;
    let path = harness._dir.path().join("test.sqlite3");
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    assert!(harness.client.get_session(session.id).await.unwrap().busy);
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    harness.wait_stopped(fork.session.id).await;
    assert_eq!(fork.agents[0].state, AgentState::Interrupted);
    harness
        .client
        .submit_request(session.id, agent.id, &message(MessageDelivery::Queue))
        .await
        .unwrap();
    // Wait until the initial partial item has actually reached the worker.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let page = harness
                .client
                .events(
                    session.id,
                    Some(agent.id),
                    &wire::EventsQuery {
                        include_pending: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            if !page.pending.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    harness.stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), harness.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(harness.requests.try_recv().is_err());
    let db = Database::open(path).await.unwrap();
    support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Interrupted
    );
    let events = cursor.load_all_events(agent.id).await.unwrap();
    assert_eq!(events.iter().filter(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Human)).count(), 1);
    assert!(events.iter().any(|event| matches!(&event.kind, EventType::Message(message) if message.message_type == MessageType::Agent)));
    assert!(events.iter().any(|event| matches!(&event.kind, EventType::InferenceResultEnd(end) if matches!(end.outcome, api::Outcome::Cancelled))));
    cursor.rollback().await.unwrap();
    db.close().await;
    harness.fake_stop.send_replace(true);
    harness.fake_server.await.unwrap();
}

fn inference_events(id: u64) -> Vec<NewEvent> {
    vec![
        NewEvent::new(EventType::InferenceResultStart(InferenceResultStart {
            id,
            model_config: model(),
            request_config: Default::default(),
        })),
        NewEvent::new(EventType::ToolCall(ToolCall {
            call_id: format!("call-{id}"),
            tool_name: "noop".into(),
            arguments: "{}".into(),
            metadata: Default::default(),
        })),
        NewEvent::new(EventType::InferenceResultEnd(InferenceResultEnd {
            id,
            outcome: api::Outcome::Completed {
                reason: api::StopReason::ToolCalls,
            },
            usage: api::Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                cached_input_tokens: Some(0),
                reasoning_output_tokens: Some(0),
                ..Default::default()
            },
            request_time: Default::default(),
            response_id: None,
            provider_metadata: Default::default(),
        })),
    ]
}

#[tokio::test]
async fn cursor_windows_fork_usage_and_idempotent_orphan_repair() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("db.sqlite3")).await.unwrap();
    support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: model(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "root".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "agent".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        cursor
            .load_context_window(agent.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        cursor
            .load_last_inference_result(agent.id)
            .await
            .unwrap()
            .is_none()
    );
    cursor
        .add_events(
            agent.id,
            &[
                NewEvent::new(EventType::NewContext),
                NewEvent::new(EventType::Model(model())),
                NewEvent::new(EventType::ToolDefinition(
                    ToolDefinition::for_tool(ToolId::Noop).unwrap(),
                )),
            ],
        )
        .await
        .unwrap();
    let added = cursor
        .add_events(agent.id, &inference_events(1))
        .await
        .unwrap();
    assert_eq!(added.new_usage.as_ref().unwrap().total_tokens, Some(15));
    let end_id = added.events.last().unwrap().id;
    let fork = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: end_id,
                },
                metadata: SessionUpdate::default(),
            },
        )
        .await
        .unwrap();
    let child = &fork.agents[0];
    assert_eq!(child.usage.total_tokens, Some(15));
    let incomplete = cursor
        .fork_session(
            session.id,
            SessionFork {
                point: ForkPoint::ThroughEvent {
                    agent_id: agent.id,
                    event_id: EventId(end_id.0 - 1),
                },
                metadata: SessionUpdate::default(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        cursor
            .load_last_inference_result(incomplete.agents[0].id)
            .await,
        Err(DbError::IncompleteInference(1))
    ));
    let logs: Vec<_> = (0..10100)
        .map(|_| {
            NewEvent::new(EventType::Log(Log {
                error: false,
                message: "log".into(),
            }))
        })
        .collect();
    cursor.add_events(agent.id, &logs).await.unwrap();
    assert_eq!(
        cursor.load_context_window(agent.id).await.unwrap().len(),
        10106
    );
    assert_eq!(
        cursor
            .load_events_since_last_inference(agent.id)
            .await
            .unwrap()
            .unwrap()
            .len(),
        10103
    );
    let response = cursor
        .load_last_inference_result(child.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.items.len(), 1);
    assert_eq!(response.end.id, end_id);
    cursor.commit().await.unwrap();

    let publication = Publication::default();
    let probes = ProbeRegistry::start(fake_probe::PROBE_KEY.into());
    let runtime = Runtime::start(
        db.clone(),
        publication,
        ProviderRegistry::default(),
        probes.clone(),
    )
    .await
    .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    let child_events = cursor.load_all_events(child.id).await.unwrap();
    let repaired_len = child_events.len();
    assert_eq!(
        child_events
            .iter()
            .filter(|event| matches!(&event.kind, EventType::ToolResult(result) if result.error))
            .count(),
        1
    );
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().state,
        AgentState::Interrupted
    );
    assert_eq!(
        cursor.get_agent(child.id).await.unwrap().usage.total_tokens,
        Some(15)
    );
    cursor.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
    let runtime = Runtime::start(
        db.clone(),
        Publication::default(),
        ProviderRegistry::default(),
        probes.clone(),
    )
    .await
    .unwrap();
    let mut cursor = db.cursor().await.unwrap();
    assert_eq!(
        cursor.load_all_events(child.id).await.unwrap().len(),
        repaired_len
    );
    cursor.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
    probes.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn persistence_failure_does_not_mutate_worker_memory() {
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    let db = Database::open(&path).await.unwrap();
    support::register_probe(&db).await;
    let mut cursor = db.cursor().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            model_config: model(),
            probe_id: psa_api::ProbeId(psa_api::probes::Uuid::nil()),
            name: "root".into(),
            working_directory: "/work".into(),
        })
        .await
        .unwrap();
    let agent = cursor
        .create_agent(
            session.id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "agent".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut state = AgentManagerState::load(db.clone(), Publication::default(), agent.id)
        .await
        .unwrap();
    let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_append BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'test failure'); END").execute(&mut connection).await.unwrap();
    assert!(
        state
            .persist_events(&[NewEvent::new(EventType::StatusChange(StatusChange {
                new_status: AgentState::Running,
                reason: None
            }))])
            .await
            .is_err()
    );
    assert_eq!(state.agent().state, AgentState::Idle);
    assert_eq!(state.context().last_event_id, EventId(0));
    let mut cursor = db.cursor().await.unwrap();
    assert!(cursor.load_all_events(agent.id).await.unwrap().is_empty());
    assert_eq!(
        cursor.get_agent(agent.id).await.unwrap().state,
        AgentState::Idle
    );
    cursor.rollback().await.unwrap();
    connection.close().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn core_ancestry_and_typed_steering_work_without_subagent_bundle_over_http_and_sse() {
    let mut harness = Harness::start(vec![Script::Text]).await;
    let (session, parent) = harness.create_agent().await;
    let child = harness
        .client
        .create_agent(
            session.id,
            &wire::NewConfiguredAgent {
                model: None,
                subagent_parent_id: Some(parent.id),
                config_id: config_id("default"),
                name: "plain-child".into(),
                interactive: false,
                hidden: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(child.subagent_parent_id, Some(parent.id));
    assert_eq!(child.parent_id, None);
    harness.wait_stopped(session.id).await;
    let mut stream = harness
        .client
        .stream_events(session.id, Some(child.id), &Default::default(), None)
        .await
        .unwrap();
    while !matches!(
        stream.next().await.unwrap().unwrap().message,
        StreamMessage::Snapshot { .. }
    ) {}
    let accepted = harness
        .client
        .send_subagent_message(
            session.id,
            child.id,
            Some(wire::AgentMessageSource {
                agent_id: Some(parent.id),
                name: Some("untrusted display name".into()),
            }),
            psa_server::prompts::text("inspect this"),
        )
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), harness.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let rendered = serde_json::to_string(&request["messages"]).unwrap();
    assert!(rendered.contains("Message from agent (agent "));
    assert!(rendered.contains("inspect this"));
    assert!(!rendered.contains("untrusted display name"));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let StreamMessage::Committed { event, .. } =
                stream.next().await.unwrap().unwrap().message
            else {
                continue;
            };
            let EventType::Message(message) = event.event.kind else {
                continue;
            };
            if message.message_type == MessageType::Subagent {
                assert_eq!(
                    message.source.unwrap(),
                    MessageSender {
                        agent_id: Some(parent.id),
                        name: Some(parent.name.clone()),
                        request_id: accepted.request_id.clone(),
                    }
                );
                break;
            }
        }
    })
    .await
    .unwrap();
    drop(stream);
    harness.wait_stopped(session.id).await;
    let loaded = harness
        .client
        .get_agent(session.id, child.id)
        .await
        .unwrap();
    assert_eq!(loaded.subagent_parent_id, Some(parent.id));
    assert!(
        harness
            .client
            .list_agents(session.id)
            .await
            .unwrap()
            .iter()
            .any(|agent| agent.id == child.id && agent.subagent_parent_id == Some(parent.id))
    );
    let page = harness
        .client
        .events(session.id, Some(child.id), &Default::default())
        .await
        .unwrap();
    assert!(page.events.iter().any(|event| matches!(&event.event.kind, EventType::Message(message) if message.message_type == MessageType::Subagent && message.source.as_ref().and_then(|source| source.agent_id) == Some(parent.id))));
    let fork = harness
        .client
        .fork_session(
            session.id,
            &wire::ForkSession {
                point: wire::ForkPoint::Head,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let copied_parent = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(parent.id))
        .unwrap()
        .id;
    let copied_child = fork
        .agents
        .iter()
        .find(|agent| agent.parent_id == Some(child.id))
        .unwrap();
    assert_eq!(copied_child.subagent_parent_id, Some(copied_parent));
    assert_eq!(copied_child.parent_id, Some(child.id));
    harness.wait_stopped(fork.session.id).await;
    harness.close().await;
}
