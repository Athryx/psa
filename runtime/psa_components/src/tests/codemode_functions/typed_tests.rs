use super::*;
use crate::test_support as support;
use llm_api::api::ToolSchema;
use psa_agents::{
    NewAgent, agent_context::AgentContext, agent_manager::AgentManagerState,
    cancellation::Cancellation, probes::ProbeRegistry, publication::Publication,
    supervisor::Runtime,
};
use psa_models::ProviderRegistry;
use psa_store::NewSession;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TestArgs {
    count: u64,
}

struct TestFunction(AtomicUsize);

impl TestFunction {
    async fn execute_typed(
        &self,
        arguments: TestArgs,
        _context: &mut AgentContext<'_>,
        _cancellation: Cancellation,
    ) -> CodeModeFunctionResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"count": arguments.count}))
    }
}

impl CodeModeFunction for TestFunction {
    fn definition(&self) -> CodeModeFunctionDefinition {
        CodeModeFunctionDefinition {
            name: "test_typed".into(),
            description: "Test typed function".into(),
            parameters: ToolSchema::for_type::<TestArgs>(),
        }
    }

    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    typed_codemode_execute!(TestArgs, execute_typed);
}

#[tokio::test]
async fn typed_bridge_validates_and_cancels_before_running() {
    let directory = tempfile::tempdir().unwrap();
    let database = psa_store::Database::open(directory.path().join("typed.sqlite3"))
        .await
        .unwrap();
    support::register_probe(&database).await;
    let publication = Publication::default();
    let probes = ProbeRegistry::start("test-probe-key".into());
    let runtime = Runtime::start(
        database.clone(),
        publication.clone(),
        ProviderRegistry::default(),
        probes.clone(),
        std::sync::Arc::new(crate::bundles::BuiltinComponents),
    )
    .await
    .unwrap();
    let mut cursor = database.write().await.unwrap();
    let session = cursor
        .create_session(NewSession {
            decision_model: None,
            model_config: support::model_config(),
            name: "typed".into(),
            working_directory: "/work".into(),
            probe_id: support::probe_metadata().id,
            project_id: None,
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
                name: "typed".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let mut state = AgentManagerState::load_persisted(database.clone(), publication, agent.id)
        .await
        .unwrap();
    let mut context = AgentContext::with_optional_probe(&mut state, None, &runtime.handle);
    let function = TestFunction(AtomicUsize::new(0));

    for (args, expected) in [
        (json!({}), "count"),
        (
            json!({"count": 1, "extra": "SECRET_VALUE"}),
            "unknown field",
        ),
        (json!({"count": "SECRET_VALUE"}), "count"),
        (json!([]), "JSON object"),
    ] {
        let (_sender, cancellation) = Cancellation::channel();
        let error = function
            .execute(args, &mut context, cancellation)
            .await
            .unwrap_err();
        assert_eq!(error.exception_type, "TypeError");
        assert!(error.message.contains(expected), "{}", error.message);
        assert!(!error.message.contains("SECRET_VALUE"));
        assert_eq!(function.0.load(Ordering::SeqCst), 0);
    }

    let (sender, cancellation) = Cancellation::channel();
    drop(sender);
    let error = function
        .execute(json!({}), &mut context, cancellation)
        .await
        .unwrap_err();
    assert_eq!(error, CodeModeError::runtime("Execution cancelled"));
    assert_eq!(function.0.load(Ordering::SeqCst), 0);

    let (_sender, cancellation) = Cancellation::channel();
    let result = function
        .execute(json!({"count": 7}), &mut context, cancellation)
        .await
        .unwrap();
    assert_eq!(result, json!({"count": 7}));
    assert_eq!(function.0.load(Ordering::SeqCst), 1);

    drop(context);
    drop(state);
    runtime.shutdown().await.unwrap();
    probes.shutdown().await;
    database.close().await;
}
