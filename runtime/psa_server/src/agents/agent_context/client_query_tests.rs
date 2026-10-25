use super::*;
use crate::agents::client_queries::ClientQueryError;
use futures_util::StreamExt;
use psa_api::{
    AskQuestionRequest, AskQuestionResponse, ClientAnswer, ClientError, ClientQuery, ClientQueryId,
    ClientQueryRequest, ClientQueryResponse, ClientQuestion, EventStream, EventsQuery,
};

fn question() -> ClientQuery {
    ClientQuery::AskQuestion(AskQuestionRequest {
        questions: vec![ClientQuestion {
            question: "Which approach?".into(),
            choices: vec!["First".into(), "Second".into()],
        }],
    })
}

fn answer() -> ClientQueryResponse {
    ClientQueryResponse::AskQuestion(AskQuestionResponse {
        answers: vec![ClientAnswer {
            answer: "A custom alternative".into(),
            notes: Some("Please keep this note".into()),
        }],
    })
}

async fn next_message(stream: &mut EventStream) -> StreamMessage {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .message
}

async fn snapshot_queries(stream: &mut EventStream) -> Vec<ClientQueryRequest> {
    loop {
        if let StreamMessage::Snapshot { queries, .. } = next_message(stream).await {
            return queries;
        }
    }
}

async fn start_query(
    fixture: &Fixture,
    agent: &Agent,
) -> (
    oneshot::Sender<()>,
    JoinHandle<Result<ClientQueryResponse, ClientQueryError>>,
    ClientQueryRequest,
) {
    let mut received = fixture.publication.lock().await.subscribe();
    let mut state = fixture.load_state(agent).await;
    let sessions = fixture.runtime.handle.clone();
    let (cancel, mut cancellation) = Cancellation::channel();
    let task = tokio::spawn(async move {
        // No connected probe or interactive flag is required for this capability.
        let context = AgentContext::with_optional_probe(&mut state, None, &sessions);
        context.query_client(question(), &mut cancellation).await
    });
    let query = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let StreamMessage::ClientQueryRequested { query } = received.recv().await.unwrap() {
                break query;
            }
        }
    })
    .await
    .unwrap();
    (cancel, task, query)
}

fn native_question_arguments() -> serde_json::Value {
    serde_json::json!({"questions":[{"question":"Which approach?","choices":["First","Second"]}]})
}

async fn start_native_question(
    fixture: &Fixture,
    agent: &Agent,
) -> (
    oneshot::Sender<()>,
    JoinHandle<ToolOutput>,
    ClientQueryRequest,
) {
    let mut received = fixture.publication.lock().await.subscribe();
    let mut state = fixture.load_state(agent).await;
    let sessions = fixture.runtime.handle.clone();
    let (cancel, cancellation) = Cancellation::channel();
    let task = tokio::spawn(async move {
        let mut context = AgentContext::with_optional_probe(&mut state, None, &sessions);
        crate::components::tools::AskQuestion
            .execute(native_question_arguments(), &mut context, cancellation)
            .await
    });
    let query = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let StreamMessage::ClientQueryRequested { query } = received.recv().await.unwrap() {
                break query;
            }
        }
    })
    .await
    .unwrap();
    (cancel, task, query)
}

#[tokio::test]
async fn native_ask_question_roundtrips_structured_answers_without_probe_or_interactive_guard() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    assert!(!agent.interactive);
    let (_cancel, waiting, query) = start_native_question(&fixture, &agent).await;
    assert_eq!(query.agent_id, agent.id);
    assert_eq!(query.session_id, agent.session_id);
    assert_eq!(query.payload, question());
    assert!(!waiting.is_finished());
    let page = fixture
        .client
        .events(agent.session_id, Some(agent.id), &EventsQuery::default())
        .await
        .unwrap();
    assert_eq!(page.queries, vec![query.clone()]);
    assert!(
        page.events.is_empty(),
        "the question tool must not write auxiliary events"
    );
    fixture
        .client
        .respond_to_client_query(agent.session_id, agent.id, query.id, &answer())
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(!output.error, "{}", native_tool_text(&output));
    let decoded: AskQuestionResponse = serde_json::from_str(native_tool_text(&output)).unwrap();
    let ClientQueryResponse::AskQuestion(expected) = answer();
    assert_eq!(decoded, expected);
    let page = fixture
        .client
        .events(agent.session_id, None, &EventsQuery::default())
        .await
        .unwrap();
    assert!(page.queries.is_empty());
    assert!(page.events.is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn cancelling_native_ask_question_removes_its_pending_client_query() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let (cancel, waiting, query) = start_native_question(&fixture, &agent).await;
    let mut received = fixture.publication.lock().await.subscribe();
    cancel.send(()).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(output.error);
    assert!(
        native_tool_text(&output)
            .to_lowercase()
            .contains("interrupted")
    );
    let removed = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(removed, StreamMessage::ClientQueryRemoved { session_id, agent_id, query_id }
        if session_id == agent.session_id && agent_id == agent.id && query_id == query.id)
    );
    let page = fixture
        .client
        .events(agent.session_id, None, &EventsQuery::default())
        .await
        .unwrap();
    assert!(page.queries.is_empty());
    assert!(page.events.is_empty());
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(agent.session_id, agent.id, query.id, &answer())
            .await,
        Err(ClientError::Api { status: 410, .. })
    ));
    fixture.close().await;
}

#[tokio::test]
async fn invalid_or_precancelled_native_questions_do_not_register_client_queries() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let mut received = fixture.publication.lock().await.subscribe();
    for arguments in [
        serde_json::json!({}),
        serde_json::json!({"questions":[]}),
        serde_json::json!({"questions":[{"question":" "}]}),
        serde_json::json!({"questions":[{"question":4}]}),
        serde_json::json!({"questions":[{"question":"Which?","choices":[""]}]}),
        serde_json::json!({"questions":[{"question":"Which?","extra":true}]}),
        serde_json::json!({"questions":[{"question":"Which?"}],"extra":true}),
    ] {
        let mut context =
            AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
        let (_cancel, cancellation) = Cancellation::channel();
        let output = crate::components::tools::AskQuestion
            .execute(arguments, &mut context, cancellation)
            .await;
        assert!(output.error);
        assert!(matches!(
            received.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
    let mut context = AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
    let (cancel, cancellation) = Cancellation::channel();
    cancel.send(()).unwrap();
    let output = crate::components::tools::AskQuestion
        .execute(native_question_arguments(), &mut context, cancellation)
        .await;
    assert!(output.error);
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(
        fixture
            .publication
            .lock()
            .await
            .queries(agent.session_id, Some(agent.id))
            .is_empty()
    );
    assert!(context.load_all_events().await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn context_client_query_roundtrip_survives_sse_reconnect_and_validates_responses() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let (_cancel, waiting, query) = start_query(&fixture, &agent).await;
    assert_eq!(query.session_id, agent.session_id);
    assert_eq!(query.agent_id, agent.id);
    assert_eq!(query.payload, question());
    assert!(!waiting.is_finished());

    let options = EventsQuery {
        include_pending: false,
        ..Default::default()
    };
    let page = fixture
        .client
        .events(agent.session_id, Some(agent.id), &options)
        .await
        .unwrap();
    assert_eq!(page.queries, [query.clone()]);
    let mut stream = fixture
        .client
        .stream_events(agent.session_id, None, &options, None)
        .await
        .unwrap();
    assert_eq!(snapshot_queries(&mut stream).await, [query.clone()]);
    drop(stream);
    assert!(!waiting.is_finished());

    let mut stream = fixture
        .client
        .stream_events(agent.session_id, Some(agent.id), &options, None)
        .await
        .unwrap();
    assert_eq!(snapshot_queries(&mut stream).await, [query.clone()]);
    let response_path =
        psa_api::routes::client_query_response_path(agent.session_id, agent.id, query.id);
    for body in [
        serde_json::json!({"type": "ask_question", "payload": {"answers": ["PRIVATE_ANSWER"]}})
            .to_string(),
        "{\"PRIVATE_ANSWER\":".into(),
    ] {
        let rejected = reqwest::Client::new()
            .post(format!("{}{response_path}", fixture.base))
            .bearer_auth("test-client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
        let error = rejected.json::<psa_api::ApiError>().await.unwrap();
        assert_eq!(error.code, "invalid_input");
        assert_eq!(error.message, "invalid client query response body");
        assert!(!error.message.contains("PRIVATE_ANSWER"));
    }

    let invalid = ClientQueryResponse::AskQuestion(AskQuestionResponse { answers: vec![] });
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(agent.session_id, agent.id, query.id, &invalid)
            .await,
        Err(ClientError::Api { status: 400, .. })
    ));
    assert!(!waiting.is_finished());
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(
                agent.session_id,
                psa_api::AgentId(agent.id.0 + 100),
                query.id,
                &answer()
            )
            .await,
        Err(ClientError::Api { status: 404, .. })
    ));
    let page = fixture
        .client
        .events(agent.session_id, None, &options)
        .await
        .unwrap();
    assert_eq!(page.queries, [query.clone()]);

    fixture
        .client
        .respond_to_client_query(agent.session_id, agent.id, query.id, &answer())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        answer()
    );
    assert!(matches!(next_message(&mut stream).await,
        StreamMessage::ClientQueryRemoved { session_id, agent_id, query_id }
        if session_id == agent.session_id && agent_id == agent.id && query_id == query.id));
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(agent.session_id, agent.id, query.id, &answer())
            .await,
        Err(ClientError::Api { status: 409, .. })
    ));
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(agent.session_id, agent.id, ClientQueryId::new(), &answer())
            .await,
        Err(ClientError::Api { status: 404, .. })
    ));
    assert!(
        fixture
            .client
            .events(agent.session_id, None, &options)
            .await
            .unwrap()
            .queries
            .is_empty()
    );
    assert!(
        fixture
            .publication
            .lock()
            .await
            .pending(agent.session_id, Some(agent.id))
            .is_empty()
    );

    drop(stream);
    fixture.close().await;
}

#[tokio::test]
async fn context_query_snapshots_are_scoped_and_cancellation_removes_the_query() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let other_session_agent = fixture.create_agent(&[]).await;
    let mut cursor = fixture.database.cursor().await.unwrap();
    let sibling = cursor
        .create_agent(
            agent.session_id,
            NewAgent {
                subagent_parent_id: None,
                interactive: false,
                hidden: false,
                definition_id: psa_api::AgentDefinitionId(1),
                name: "sibling".into(),
            },
        )
        .await
        .unwrap();
    cursor.commit().await.unwrap();
    let (cancel, waiting, query) = start_query(&fixture, &agent).await;
    let options = EventsQuery {
        include_pending: false,
        ..Default::default()
    };
    for (session, agent) in [
        (sibling.session_id, sibling.id),
        (other_session_agent.session_id, other_session_agent.id),
    ] {
        let page = fixture
            .client
            .events(session, Some(agent), &options)
            .await
            .unwrap();
        assert!(page.queries.is_empty());
        let mut stream = fixture
            .client
            .stream_events(session, Some(agent), &options, None)
            .await
            .unwrap();
        assert!(snapshot_queries(&mut stream).await.is_empty());
    }
    let mut stream = fixture
        .client
        .stream_events(agent.session_id, Some(agent.id), &options, None)
        .await
        .unwrap();
    assert_eq!(snapshot_queries(&mut stream).await, [query.clone()]);
    cancel.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(ClientQueryError::Cancelled)
    ));
    assert!(
        matches!(next_message(&mut stream).await, StreamMessage::ClientQueryRemoved { query_id, .. } if query_id == query.id)
    );
    assert!(matches!(
        fixture
            .client
            .respond_to_client_query(agent.session_id, agent.id, query.id, &answer())
            .await,
        Err(ClientError::Api { status: 410, .. })
    ));
    assert!(
        fixture
            .client
            .events(agent.session_id, None, &options)
            .await
            .unwrap()
            .queries
            .is_empty()
    );
    drop(stream);
    fixture.close().await;
}

#[tokio::test]
async fn runtime_shutdown_releases_context_queries_without_a_client_response() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let (_cancel, waiting, query) = start_query(&fixture, &agent).await;
    let mut received = fixture.publication.lock().await.subscribe();
    let Fixture {
        _directory,
        database,
        publication,
        client: _,
        base: _,
        runtime,
        probes,
        probe,
        stop,
        server,
    } = fixture;
    let sessions = runtime.handle.clone();

    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap(),
        Err(ClientQueryError::Unavailable)
    ));
    assert!(
        publication
            .lock()
            .await
            .queries(agent.session_id, Some(agent.id))
            .is_empty()
    );
    let removal = received.recv().await.unwrap();
    assert!(
        matches!(removal, StreamMessage::ClientQueryRemoved { query_id, .. } if query_id == query.id)
    );
    assert!(matches!(
        sessions
            .respond_to_client_query(agent.session_id, agent.id, query.id, answer())
            .await,
        Err(ClientQueryError::Unavailable)
    ));

    probe.disconnect(&probes).await;
    probes.shutdown().await;
    let _ = stop.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    database.close().await;
}

struct AskingHook;

impl Hook for AskingHook {
    fn cancellation_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }

    async fn run(
        &mut self,
        context: &mut AgentContext<'_>,
        mut cancellation: Cancellation,
    ) -> Result<(), HookError> {
        match context.query_client(question(), &mut cancellation).await {
            Ok(_) | Err(ClientQueryError::Cancelled) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[tokio::test]
async fn hook_interrupt_cancels_its_client_query_during_the_cleanup_grace_period() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let mut state = fixture.load_state(&agent).await;
    let mut events = fixture.publication.lock().await.subscribe();
    let (control, mut controls) = actors::unbounded_channel();
    let mut hook = AskingHook;
    let query_id = {
        let mut context =
            AgentContext::with_optional_probe(&mut state, None, &fixture.runtime.handle);
        let invocation = run_hook(&mut hook, &mut context, &mut controls);
        tokio::pin!(invocation);
        assert!(futures_util::poll!(&mut invocation).is_pending());
        let requested = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let StreamMessage::ClientQueryRequested { query } = requested else {
            panic!("expected query request");
        };

        control
            .send(crate::agents::agent_manager::AgentManagerMessage::Interrupt)
            .unwrap();
        let interrupted = tokio::time::timeout(Duration::from_secs(5), invocation)
            .await
            .unwrap()
            .unwrap();
        assert!(interrupted);
        query.id
    };
    let removed = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(removed, StreamMessage::ClientQueryRemoved { query_id: id, .. } if id == query_id)
    );
    assert!(
        fixture
            .publication
            .lock()
            .await
            .queries(agent.session_id, Some(agent.id))
            .is_empty()
    );
    fixture.close().await;
}

#[tokio::test]
async fn aborting_context_query_future_removes_pending_client_ui() {
    let fixture = Fixture::new().await;
    let agent = fixture.create_agent(&[]).await;
    let (_cancel, waiting, query) = start_query(&fixture, &agent).await;
    let mut received = fixture.publication.lock().await.subscribe();
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let removal = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(removal, StreamMessage::ClientQueryRemoved { query_id, .. } if query_id == query.id)
    );
    assert!(
        fixture
            .client
            .events(agent.session_id, None, &Default::default())
            .await
            .unwrap()
            .queries
            .is_empty()
    );
    fixture.close().await;
}
