use psa_api::{ProbeId, ProbeMetadata, probes::Uuid};
use psa_server::db::Database;

pub fn probe_metadata() -> ProbeMetadata {
    ProbeMetadata {
        id: ProbeId(Uuid::nil()),
        hostname: "test".into(),
        os: "linux".into(),
        architecture: "x86_64".into(),
        label: None,
        data_dir: "/tmp/psa-test".into(),
    }
}

#[allow(dead_code)]
pub fn model_config() -> llm_api::providers::ModelConfig {
    use llm_api::providers::{Effort, Model, ModelConfig, ProviderType};
    ModelConfig {
        model: Model::new(ProviderType::Openrouter, "test-model").unwrap(),
        effort: Effort::Medium,
    }
}

pub async fn create_agent_definition(
    cursor: &mut psa_server::db::DatabaseCursor,
) -> psa_api::AgentDefinitionId {
    cursor
        .create_agent_definition(psa_api::CreateAgentDefinition {
            name: "default".into(),
            tools: vec![psa_api::ToolId::Noop],
            hooks: psa_api::HookIds {
                agent_create: vec![
                    psa_api::HookId::DefaultStartup,
                    psa_api::HookId::ToolDefinitions,
                ],
                session_update: vec![psa_api::HookId::SessionModelUpdate],
                after_request: vec![psa_api::HookId::StopWithoutTools],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub async fn register_probe(database: &Database) {
    let mut cursor = database.cursor().await.unwrap();
    cursor.upsert_probe(&probe_metadata()).await.unwrap();
    if cursor.list_agent_definitions().await.unwrap().is_empty() {
        create_agent_definition(&mut cursor).await;
    }
    cursor.commit().await.unwrap();
}
