use super::*;
use psa::api::{AskQuestionRequest, AskQuestionResponse, ClientAnswer, ClientQuestion};
use tokio::sync::broadcast;

const SESSION: SessionId = SessionId(1);
const AGENT: AgentId = AgentId(2);

fn question() -> ClientQuery {
    ClientQuery::AskQuestion(AskQuestionRequest {
        questions: vec![ClientQuestion {
            question: "Choose a direction".into(),
            choices: vec!["left".into(), "right".into()],
        }],
    })
}

fn answer() -> ClientQueryResponse {
    ClientQueryResponse::AskQuestion(AskQuestionResponse {
        answers: vec![ClientAnswer {
            answer: "elsewhere".into(),
            notes: None,
        }],
    })
}

async fn requested(events: &mut broadcast::Receiver<StreamMessage>) -> ClientQueryRequest {
    match events.recv().await.unwrap() {
        StreamMessage::ClientQueryRequested { query } => query,
        event => panic!("unexpected event: {event:?}"),
    }
}

async fn removed(events: &mut broadcast::Receiver<StreamMessage>, id: ClientQueryId) {
    assert!(
        matches!(events.recv().await.unwrap(), StreamMessage::ClientQueryRemoved {
        session_id: SESSION, agent_id: AGENT, query_id,
    } if query_id == id)
    );
}

#[tokio::test]
async fn first_valid_response_wins_and_scope_is_checked_for_pending_and_tombstones() {
    let publication = Publication::default();
    let mut events = publication.lock().await.subscribe();
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (_sender, mut cancellation) = Cancellation::channel();
    let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    let request = requested(&mut events).await;
    assert_eq!(
        publication.lock().await.queries.get(&request.id),
        Some(&request)
    );

    for (session, agent) in [(SessionId(9), AGENT), (SESSION, AgentId(9))] {
        assert_eq!(
            handle.respond(session, agent, request.id, answer()).await,
            Err(ClientQueryError::NotFound)
        );
    }
    let invalid = ClientQueryResponse::AskQuestion(AskQuestionResponse { answers: vec![] });
    assert!(matches!(
        handle.respond(SESSION, AGENT, request.id, invalid).await,
        Err(ClientQueryError::InvalidResponse(_))
    ));
    assert!(publication.lock().await.queries.contains_key(&request.id));
    assert!(events.try_recv().is_err());

    handle
        .respond(SESSION, AGENT, request.id, answer())
        .await
        .unwrap();
    assert_eq!(query.await, Ok(answer()));
    removed(&mut events, request.id).await;
    assert!(publication.lock().await.queries.is_empty());
    assert_eq!(
        handle.respond(SESSION, AGENT, request.id, answer()).await,
        Err(ClientQueryError::AlreadyResolved)
    );
    assert_eq!(
        handle
            .respond(SessionId(9), AGENT, request.id, answer())
            .await,
        Err(ClientQueryError::NotFound)
    );
    assert_eq!(
        handle
            .respond(SESSION, AgentId(9), request.id, answer())
            .await,
        Err(ClientQueryError::NotFound)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn already_cancelled_and_invalid_requests_are_not_published() {
    let publication = Publication::default();
    let mut events = publication.lock().await.subscribe();
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (sender, mut cancellation) = Cancellation::channel();
    sender.send(()).unwrap();
    assert_eq!(
        handle
            .query(SESSION, AGENT, question(), &mut cancellation)
            .await,
        Err(ClientQueryError::Cancelled)
    );
    let (_sender, mut cancellation) = Cancellation::channel();
    let invalid = ClientQuery::AskQuestion(AskQuestionRequest { questions: vec![] });
    assert!(matches!(
        handle
            .query(SESSION, AGENT, invalid, &mut cancellation)
            .await,
        Err(ClientQueryError::InvalidRequest(_))
    ));
    assert!(publication.lock().await.queries.is_empty());
    assert!(events.try_recv().is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_removes_pending_and_retains_only_scoped_cancelled_status() {
    let publication = Publication::default();
    let mut events = publication.lock().await.subscribe();
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (sender, mut cancellation) = Cancellation::channel();
    let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    let request = requested(&mut events).await;
    sender.send(()).unwrap();
    assert_eq!(query.await, Err(ClientQueryError::Cancelled));
    removed(&mut events, request.id).await;
    assert!(publication.lock().await.queries.is_empty());
    assert_eq!(
        handle.respond(SESSION, AGENT, request.id, answer()).await,
        Err(ClientQueryError::Cancelled)
    );
    assert_eq!(
        handle
            .respond(SessionId(9), AGENT, request.id, answer())
            .await,
        Err(ClientQueryError::NotFound)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_a_waiting_future_cancels_its_pending_query() {
    let publication = Publication::default();
    let mut events = publication.lock().await.subscribe();
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (_sender, mut cancellation) = Cancellation::channel();
    let request;
    {
        let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
        tokio::pin!(query);
        assert!(futures_util::poll!(&mut query).is_pending());
        request = requested(&mut events).await;
        // Poll through registration acknowledgement into the human-reply wait.
        assert!(futures_util::poll!(&mut query).is_pending());
    }
    removed(&mut events, request.id).await;
    assert!(publication.lock().await.queries.is_empty());
    assert_eq!(
        handle.respond(SESSION, AGENT, request.id, answer()).await,
        Err(ClientQueryError::Cancelled)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_registration_does_not_publish_when_queued_or_waiting_for_gate() {
    for let_actor_reach_gate in [false, true] {
        let publication = Publication::default();
        let gate = publication.lock().await;
        let mut events = gate.subscribe();
        let mut runtime = ClientQueries::start(publication.clone());
        let handle = runtime.handle();
        let (_sender, mut cancellation) = Cancellation::channel();
        {
            let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
            tokio::pin!(query);
            assert!(futures_util::poll!(&mut query).is_pending());
            if let_actor_reach_gate {
                tokio::task::yield_now().await;
            }
        }
        drop(gate);
        // A request behind Register + Cancel is a deterministic mailbox barrier.
        assert_eq!(
            handle
                .respond(SESSION, AGENT, ClientQueryId::new(), answer())
                .await,
            Err(ClientQueryError::NotFound)
        );
        assert!(publication.lock().await.queries.is_empty());
        assert!(events.try_recv().is_err());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn shutdown_wakes_waiters_rejects_new_requests_and_is_idempotent() {
    let publication = Publication::default();
    let mut events = publication.lock().await.subscribe();
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (_sender, mut cancellation) = Cancellation::channel();
    let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    let request = requested(&mut events).await;
    runtime.shutdown().await.unwrap();
    assert_eq!(query.await, Err(ClientQueryError::Unavailable));
    removed(&mut events, request.id).await;
    assert!(publication.lock().await.queries.is_empty());
    runtime.shutdown().await.unwrap();
    let (_sender, mut cancellation) = Cancellation::channel();
    assert_eq!(
        handle
            .query(SESSION, AGENT, question(), &mut cancellation)
            .await,
        Err(ClientQueryError::Unavailable)
    );
    assert_eq!(
        handle.respond(SESSION, AGENT, request.id, answer()).await,
        Err(ClientQueryError::Unavailable)
    );
}

#[tokio::test]
async fn cancelled_shutdown_keeps_ownership_until_a_later_shutdown() {
    let publication = Publication::default();
    let gate = publication.lock().await;
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    let (_sender, mut cancellation) = Cancellation::channel();
    let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    tokio::task::yield_now().await;
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
    }
    assert!(runtime.task.is_some());
    drop(gate);
    runtime.shutdown().await.unwrap();
    assert_eq!(query.await, Err(ClientQueryError::Unavailable));
    assert!(publication.lock().await.queries.is_empty());
}

#[tokio::test]
async fn dropping_runtime_aborts_instead_of_detaching() {
    let publication = Publication::default();
    let runtime = ClientQueries::start(publication);
    let abort = runtime.task.as_ref().unwrap().abort_handle();
    let handle = runtime.handle();
    drop(runtime);
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
    assert_eq!(
        handle
            .respond(SESSION, AGENT, ClientQueryId::new(), answer())
            .await,
        Err(ClientQueryError::Unavailable)
    );
}

#[tokio::test]
async fn pending_capacity_rejects_registration_without_publishing() {
    let publication = Publication::default();
    let mut registry = Registry {
        publication: publication.clone(),
        pending: HashMap::new(),
        tombstones: HashMap::new(),
        tombstone_order: VecDeque::new(),
    };
    let mut waiting = Vec::new();
    for _ in 0..PENDING_LIMIT {
        let (registered, registration) = oneshot::channel();
        let (waiter, response) = oneshot::channel();
        registry
            .register(
                ClientQueryRequest {
                    id: ClientQueryId::new(),
                    session_id: SESSION,
                    agent_id: AGENT,
                    payload: question(),
                },
                registered,
                waiter,
            )
            .await;
        assert_eq!(registration.await.unwrap(), Ok(()));
        waiting.push(response);
    }
    let mut events = publication.lock().await.subscribe();
    let (registered, registration) = oneshot::channel();
    let (waiter, _response) = oneshot::channel();
    registry
        .register(
            ClientQueryRequest {
                id: ClientQueryId::new(),
                session_id: SESSION,
                agent_id: AGENT,
                payload: question(),
            },
            registered,
            waiter,
        )
        .await;
    assert_eq!(
        registration.await.unwrap(),
        Err(ClientQueryError::Overloaded)
    );
    assert_eq!(registry.pending.len(), PENDING_LIMIT);
    assert_eq!(publication.lock().await.queries.len(), PENDING_LIMIT);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn shutdown_drops_queued_registration_replies() {
    let publication = Publication::default();
    let gate = publication.lock().await;
    let mut runtime = ClientQueries::start(publication.clone());
    let handle = runtime.handle();
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
    }
    let (_sender, mut cancellation) = Cancellation::channel();
    let query = handle.query(SESSION, AGENT, question(), &mut cancellation);
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    drop(gate);
    runtime.shutdown().await.unwrap();
    assert_eq!(query.await, Err(ClientQueryError::Unavailable));
    assert!(publication.lock().await.queries.is_empty());
}

#[tokio::test]
async fn tombstones_are_bounded_and_do_not_retain_request_or_answer_payloads() {
    let mut registry = Registry {
        publication: Publication::default(),
        pending: HashMap::new(),
        tombstones: HashMap::new(),
        tombstone_order: VecDeque::new(),
    };
    let first = ClientQueryId::new();
    for index in 0..=TOMBSTONE_LIMIT {
        let request = ClientQueryRequest {
            id: if index == 0 {
                first
            } else {
                ClientQueryId::new()
            },
            session_id: SESSION,
            agent_id: AGENT,
            payload: question(),
        };
        registry.remember(&request, Resolution::Answered);
    }
    assert_eq!(registry.tombstones.len(), TOMBSTONE_LIMIT);
    assert_eq!(registry.tombstone_order.len(), TOMBSTONE_LIMIT);
    assert_eq!(
        registry.respond(SESSION, AGENT, first, answer()).await,
        Err(ClientQueryError::NotFound)
    );
}
