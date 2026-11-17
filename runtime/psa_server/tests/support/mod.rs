use psa::api::{ProbeId, ProbeMetadata, probes::Uuid};
use psa_server::db::Database;

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
) -> psa::api::AgentDefinitionId {
    cursor
        .create_agent_definition(psa_server::db::CreateAgentDefinition {
            name: "default".into(),
            bundles: vec![psa::api::BundleConfig::Noop],
            hooks: psa::api::HookConfigs {
                agent_create: vec![
                    psa::api::HookConfig::DefaultStartup,
                    psa::api::HookConfig::ToolDefinitions,
                ],
                session_update: vec![psa::api::HookConfig::SessionModelUpdate],
                after_request: vec![psa::api::HookConfig::StopWithoutTools],
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
