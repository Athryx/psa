//! Fixtures for tests that need a populated store.

use crate::{Agent, CreateAgentDefinition, Database, NewAgent, NewSession, Session, WriteCursor};
use psa::api::{AgentDefinitionId, BundleConfig, ProbeId, ProbeMetadata, probes::Uuid};

pub fn probe_metadata() -> ProbeMetadata {
    ProbeMetadata {
        id: ProbeId(Uuid::nil()),
        hostname: "test".into(),
        os: "linux".into(),
        architecture: "x86_64".into(),
        label: None,
        data_dir: "/tmp/psa-test".into(),
        home_dir: None,
        host: false,
        sandboxes: false,
    }
}

pub fn model_config() -> llm_api::providers::ModelConfig {
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::Medium,
    }
}

pub async fn create_agent_definition(cursor: &mut WriteCursor) -> AgentDefinitionId {
    cursor
        .create_agent_definition(CreateAgentDefinition {
            name: "default".into(),
            bundles: vec![
                BundleConfig::Startup {
                    base_prompt: true,
                    project_context: false,
                    cwd: false,
                    skills: false,
                },
                BundleConfig::FileTools { read_only: true },
                BundleConfig::StopWithoutTools,
            ],
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

/// Register the nil test probe and a default agent definition if none exists.
pub async fn register_probe(database: &Database) {
    let mut cursor = database.write().await.unwrap();
    cursor.upsert_probe(&probe_metadata()).await.unwrap();
    if cursor.list_agent_definitions().await.unwrap().is_empty() {
        create_agent_definition(&mut cursor).await;
    }
    cursor.commit().await.unwrap();
}

/// A fresh database in a temporary directory holding one session with a root agent.
pub async fn session_fixture() -> (tempfile::TempDir, Database, Session, Agent) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("events.sqlite3"))
        .await
        .unwrap();
    register_probe(&db).await;
    let mut cursor = db.write().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            model_config: model_config(),
            probe_id: psa::api::ProbeId(psa::api::probes::Uuid::nil()),
            project_id: None,
            name: "test".into(),
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
                definition_id: psa::api::AgentDefinitionId(1),
                name: "main".into(),
            },
        )
        .await
        .unwrap();
    let session = cursor.get_session(session.id).await.unwrap();
    cursor.commit().await.unwrap();
    (dir, db, session, agent)
}
