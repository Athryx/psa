use crate::app::Ui;
use client_shared::*;
use dioxus::prelude::*;
use futures_util::StreamExt;
use psa::api::*;
use std::collections::HashMap;
use tokio::time::{Duration, Instant};

/// Transport cursors and request epochs do not invalidate UI selectors.
#[derive(Default)]
pub(crate) struct NetworkState {
    revisions: HashMap<SessionId, u64>,
    list_request: u64,
    probe_request: u64,
    selection_request: u64,
}

fn set_connection(mut ui: Signal<Ui>, connection: &str) {
    if ui.peek().connection != connection {
        ui.write().connection = connection.into();
    }
}

fn sessions_match(left: &Session, right: &Session) -> bool {
    left.id == right.id
        && left.forked_from == right.forked_from
        && left.name == right.name
        && left.working_directory == right.working_directory
        && left.probe_id == right.probe_id
        && left.model_config == right.model_config
        && left.busy == right.busy
        && left.created_at == right.created_at
        && left.updated_at == right.updated_at
}

fn session_changes(state: &Ui, session: &Session) -> bool {
    !state.fresh_sessions.contains(&session.id)
        || state
            .sessions
            .iter()
            .find(|old| old.id == session.id)
            .is_none_or(|old| !sessions_match(old, session))
}

fn collect_summary_updates(
    state: &Ui,
    updates: impl IntoIterator<Item = (u64, Session)>,
) -> Vec<Session> {
    let mut network = state.network.borrow_mut();
    let mut accepted = Vec::<Session>::new();
    for (revision, session) in updates {
        if network
            .revisions
            .get(&session.id)
            .is_some_and(|old| *old > revision)
        {
            continue;
        }
        network.revisions.insert(session.id, revision);
        // Preserve the first insertion's list position, but keep the final
        // accepted snapshot, including equal-revision replacements.
        if let Some(old) = accepted.iter_mut().find(|old| old.id == session.id) {
            *old = session;
        } else {
            accepted.push(session);
        }
    }
    accepted
        .into_iter()
        .filter(|session| session_changes(state, session))
        .collect()
}

fn apply_summary_updates(mut ui: Signal<Ui>, updates: Vec<(u64, Session)>) -> bool {
    let updates = collect_summary_updates(&ui.peek(), updates);
    if updates.is_empty() {
        return false;
    }
    let mut state = ui.write();
    for session in updates {
        state.upsert_session(session);
    }
    true
}

fn publish_summary_batch(ui: Signal<Ui>, generation: u64, batch: &mut Vec<(u64, Session)>) -> bool {
    let updates = std::mem::take(batch);
    if ui.peek().generation != generation {
        return false;
    }
    apply_summary_updates(ui, updates)
}

pub fn client(ui: Signal<Ui>) -> Result<Client, ClientError> {
    let state = ui.read();
    let connection = &state.settings.active_profile().connection;
    Client::new(&connection.api_url, &connection.client_key)
}

fn disconnected(error: &ClientError) -> &'static str {
    match error {
        ClientError::Api {
            status: 401 | 403, ..
        } => "Authentication failed — check the API client key in settings",
        ClientError::Api { status: 404, .. } => "Unavailable — this session or agent was not found",
        _ if !can_retry_connection(error) => "Connection rejected — check the API URL and settings",
        _ => "Disconnected — reconnecting…",
    }
}

/// Subscribe before fetching summaries. The feed buffers while HTTP is in
/// flight. Per-ID revisions prevent late pages from overwriting a live upsert.
pub async fn summaries(ui: Signal<Ui>, generation: u64) {
    let mut backoff = ReconnectBackoff::default();
    loop {
        if ui.read().generation != generation {
            return;
        }
        set_connection(ui, "Connecting…");
        let result = summary_connection(ui, generation).await;
        if ui.read().generation != generation {
            return;
        }
        if let Err(error) = result {
            if ui.read().connection == "Connected" {
                backoff.reset();
            }
            set_connection(ui, disconnected(&error));
            if !can_retry_connection(&error) {
                return;
            }
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

async fn summary_connection(mut ui: Signal<Ui>, generation: u64) -> Result<(), ClientError> {
    let api = client(ui)?;
    let mut feed = api
        .stream_global_events(&GlobalEventsQuery::default(), None)
        .await?;
    match feed.next().await {
        Some(Ok(GlobalStreamEvent {
            message: GlobalStreamMessage::Ready { .. },
            ..
        })) => {}
        Some(Err(error)) => return Err(error),
        _ => return Err(ClientError::Stream("No ready boundary".into())),
    }
    if ui.read().generation != generation {
        return Ok(());
    }
    let request = {
        let state = ui.peek();
        let mut network = state.network.borrow_mut();
        network.revisions.clear();
        network.list_request += 1;
        network.list_request
    };
    if !ui.peek().fresh_sessions.is_empty() {
        ui.write().fresh_sessions.clear();
    }
    let filter = ui.read().session_filter.clone();
    let page = api
        .list_sessions(&SessionsQuery {
            flat: true,
            filter: filter.clone(),
            limit: 100,
            ..Default::default()
        })
        .await?;
    if ui.read().generation != generation {
        return Ok(());
    }
    if ui.read().session_filter == filter && ui.peek().network.borrow().list_request == request {
        merge_page(ui, page);
    }
    refresh_probes(ui).await;
    refresh_projects(ui).await;
    if ui.peek().generation != generation {
        return Ok(());
    }
    if let Ok(models) = api.list_models(&ListModels::default()).await {
        if ui.read().generation != generation {
            return Ok(());
        }
        apply_models(ui, models);
    }
    if ui.read().generation != generation {
        return Ok(());
    }
    set_connection(ui, "Connected");
    // Probe connections are not included in the global session event feed.
    // Reconcile periodically, including when a probe reconnects after the server.
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(5));
    refresh.tick().await;
    let mut probe_refresh: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = None;
    let mut batch = Vec::new();
    let mut deadline = Instant::now() + STREAM_FRAME;
    loop {
        if ui.peek().generation != generation {
            return Ok(());
        }
        let event = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline), if !batch.is_empty() => {
                publish_summary_batch(ui, generation, &mut batch);
                deadline = Instant::now() + STREAM_FRAME;
                continue;
            }
            event = feed.next(), if batch.len() < MAX_STREAM_BATCH => match event {
                Some(Ok(event)) => event,
                result => {
                    publish_summary_batch(ui, generation, &mut batch);
                    return Err(match result {
                        Some(Err(error)) => error,
                        _ => ClientError::Stream("Summary stream ended".into()),
                    });
                }
            },
            _ = refresh.tick(), if probe_refresh.is_none() => {
                probe_refresh = Some(Box::pin(refresh_probes(ui)));
                continue;
            }
            _ = async { probe_refresh.as_mut().unwrap().await }, if probe_refresh.is_some() => {
                probe_refresh = None;
                continue;
            }
        };
        if ui.read().generation != generation {
            return Ok(());
        }
        match event.message {
            GlobalStreamMessage::SessionUpdated { revision, session } => {
                if batch.is_empty() {
                    deadline = Instant::now() + STREAM_FRAME;
                }
                let lifecycle = ui
                    .peek()
                    .sessions
                    .iter()
                    .find(|old| old.id == session.id)
                    .is_none_or(|old| old.busy != session.busy);
                batch.push((revision, session));
                if lifecycle {
                    publish_summary_batch(ui, generation, &mut batch);
                    deadline = Instant::now() + STREAM_FRAME;
                }
            }
            GlobalStreamMessage::ResyncRequired => {
                publish_summary_batch(ui, generation, &mut batch);
                return Err(ClientError::Stream("Summary resync requested".into()));
            }
            GlobalStreamMessage::Ready { .. } => {}
        }
    }
}

fn merge_page(mut ui: Signal<Ui>, page: SessionsPage) {
    let updates = collect_summary_updates(
        &ui.peek(),
        page.sessions
            .into_iter()
            .map(|session| (page.revision, session)),
    );
    if updates.is_empty() && ui.peek().next_sessions == page.next_before {
        return;
    }
    let mut state = ui.write();
    for session in updates {
        state.upsert_session(session);
    }
    state.next_sessions = page.next_before;
}

pub async fn load_sessions(mut ui: Signal<Ui>, older: bool) {
    let request = {
        let state = ui.peek();
        let mut network = state.network.borrow_mut();
        network.list_request += 1;
        network.list_request
    };
    let (generation, filter, cursor) = {
        let state = ui.read();
        (
            state.generation,
            state.session_filter.clone(),
            if older {
                state.next_sessions.clone()
            } else {
                None
            },
        )
    };
    if older && cursor.is_none() {
        return;
    }
    let Ok(api) = client(ui) else {
        return;
    };
    match api
        .list_sessions(&SessionsQuery {
            flat: true,
            filter: filter.clone(),
            before: cursor,
            limit: 100,
            ..Default::default()
        })
        .await
    {
        Ok(page)
            if generation == ui.read().generation
                && filter == ui.read().session_filter
                && request == ui.peek().network.borrow().list_request =>
        {
            merge_page(ui, page)
        }
        Err(_) if generation == ui.read().generation => {
            ui.write().error = Some("Unable to load session summaries".into())
        }
        _ => {}
    }
}
/// Reload the first session page after the user stops typing a name search.
/// Displayed sessions are filtered locally meanwhile, so typing stays responsive.
pub async fn search_sessions(ui: Signal<Ui>) {
    let search = ui.peek().session_filter.name_contains.clone();
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    if ui.peek().session_filter.name_contains == search {
        load_sessions(ui, false).await;
    }
}

/// Reload the projects offered by the session filter. Failures keep the old list.
pub async fn refresh_projects(mut ui: Signal<Ui>) {
    let generation = ui.peek().generation;
    let Ok(api) = client(ui) else { return };
    let Ok(projects) = api.list_projects().await else {
        return;
    };
    if ui.peek().generation == generation && *ui.peek().projects != projects {
        ui.write().projects = projects.into();
    }
}

pub async fn more_sessions(ui: Signal<Ui>) {
    load_sessions(ui, true).await;
}

pub async fn refresh_probes(ui: Signal<Ui>) {
    let generation = ui.read().generation;
    let request = {
        let state = ui.peek();
        let mut network = state.network.borrow_mut();
        network.probe_request += 1;
        network.probe_request
    };
    let result = match client(ui) {
        Ok(api) => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), api.list_probes()).await {
                Ok(result) => result.map_err(|error| format!("Unable to refresh probes: {error}")),
                Err(_) => Err("Timed out refreshing probes".into()),
            }
        }
        Err(error) => Err(format!("Unable to refresh probes: {error}")),
    };
    publish_probe_refresh(ui, generation, request, result);
    refresh_sandboxes(ui).await;
}

/// Reload sandbox state; probe polling drives this since there is no sandbox event feed.
pub async fn refresh_sandboxes(mut ui: Signal<Ui>) {
    let generation = ui.peek().generation;
    let Ok(api) = client(ui) else { return };
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(10), api.list_sandboxes(None)).await;
    let Ok(Ok(sandboxes)) = result else { return };

    if ui.peek().generation != generation || *ui.peek().sandboxes == sandboxes {
        return;
    }
    ui.write().sandboxes = sandboxes.into();
}

fn apply_sandbox(state: &mut Ui, sandbox: Sandbox) {
    let sandboxes = &mut *state.sandboxes;
    match sandboxes
        .iter_mut()
        .find(|item| item.probe_id == sandbox.probe_id)
    {
        Some(existing) => *existing = sandbox,
        None => sandboxes.push(sandbox),
    }
}

fn describe_api_error(error: &ClientError) -> String {
    match error {
        ClientError::Api { error, .. } => error.message.clone(),
        other => other.to_string(),
    }
}

/// Create a sandbox on a manager probe and add it to the probe pane.
pub async fn create_sandbox(
    mut ui: Signal<Ui>,
    manager: ProbeId,
    request: CreateSandbox,
) -> Result<Sandbox, String> {
    let generation = ui.peek().generation;
    let api = client(ui).map_err(|error| error.to_string())?;
    let sandbox = api
        .create_sandbox(manager, &request)
        .await
        .map_err(|error| describe_api_error(&error))?;

    if ui.peek().generation == generation {
        apply_sandbox(&mut ui.write(), sandbox.clone());
        spawn(refresh_probes(ui));
    }
    Ok(sandbox)
}

/// Start, stop, or delete a sandbox, reporting failures in the status bar.
/// Rename a session from the sidebar, applying the server's copy at once
/// rather than waiting for the session feed.
pub async fn rename_session(mut ui: Signal<Ui>, id: SessionId, name: String) {
    let generation = ui.peek().generation;
    let api = match client(ui) {
        Ok(api) => api,
        Err(error) => {
            ui.write().error = Some(error.to_string());
            return;
        }
    };
    let request = UpdateSession {
        name: Some(name),
        ..Default::default()
    };
    let result = api.update_session(id, &request).await;
    if ui.peek().generation != generation {
        return;
    }
    match result {
        Ok(session) => ui.write().upsert_session(session),
        Err(error) => ui.write().error = Some(format!("Unable to rename the session: {error}")),
    }
}

pub async fn change_sandbox(ui: Signal<Ui>, id: ProbeId, action: client_shared::SandboxChange) {
    apply_sandbox_change(ui, id, action).await;
}

async fn apply_sandbox_change(
    mut ui: Signal<Ui>,
    id: ProbeId,
    action: client_shared::SandboxChange,
) -> CommandEnd {
    let generation = ui.peek().generation;
    let api = match client(ui) {
        Ok(api) => api,
        Err(error) => {
            ui.write().error = Some(error.to_string());
            return Some(Err(error.to_string()));
        }
    };
    if !ui.write().sandbox_pending.insert(id) {
        return Some(Err("a change to this sandbox is already in progress".into()));
    }

    let result = match action {
        client_shared::SandboxChange::Start => api.start_sandbox(id).await.map(Some),
        client_shared::SandboxChange::Stop => api.stop_sandbox(id).await.map(Some),
        client_shared::SandboxChange::Delete => api.delete_sandbox(id).await.map(|()| None),
    };
    if ui.peek().generation != generation {
        return None;
    }

    let mut state = ui.write();
    state.sandbox_pending.remove(&id);
    match result {
        Ok(Some(sandbox)) => {
            state.notice = Some(format!("Sandbox {} {}", sandbox.name, action.past_tense()));
            apply_sandbox(&mut state, sandbox);
        }
        Ok(None) => {
            // The server keeps deleted sandboxes so they stay under their manager.
            if let Some(sandbox) = state.sandboxes.iter_mut().find(|s| s.probe_id == id) {
                sandbox.state = SandboxState::Deleted;
                sandbox.probe_active = false;
            }
            state.notice = Some("Sandbox deleted".into());
        }
        Err(error) => {
            let message = format!(
                "Unable to {} sandbox: {}",
                action.as_str(),
                describe_api_error(&error)
            );
            state.error = Some(message.clone());
            return Some(Err(message));
        }
    }
    Some(Ok(()))
}

/// Start or stop the sandbox running a session, as requested by `/sandbox`.
pub async fn change_session_sandbox(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    action: client_shared::SandboxAction,
) -> CommandEnd {
    let probe_id = ui
        .peek()
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .map(|session| session.probe_id);
    let sandbox = probe_id.and_then(|probe_id| {
        ui.peek()
            .sandboxes
            .iter()
            .any(|sandbox| sandbox.probe_id == probe_id)
            .then_some(probe_id)
    });
    let Some(sandbox) = sandbox else {
        let message = "This session is not running in a sandbox";
        ui.write().error = Some(message.into());
        return Some(Err(message.into()));
    };

    let change = match action {
        client_shared::SandboxAction::Start => client_shared::SandboxChange::Start,
        client_shared::SandboxAction::Stop => client_shared::SandboxChange::Stop,
    };
    apply_sandbox_change(ui, sandbox, change).await
}

fn publish_probe_refresh(
    mut ui: Signal<Ui>,
    generation: u64,
    request: u64,
    result: Result<Vec<Probe>, String>,
) -> bool {
    if !probe_refresh_changes(&ui.peek(), generation, request, &result) {
        return false;
    }
    apply_probe_refresh(&mut ui.write(), generation, request, result);
    true
}

fn probes_match(left: &Probe, right: &Probe) -> bool {
    let (a, b) = (&left.metadata, &right.metadata);
    a.id == b.id
        && a.hostname == b.hostname
        && a.os == b.os
        && a.architecture == b.architecture
        && a.label == b.label
        && a.data_dir == b.data_dir
        && a.home_dir == b.home_dir
        && a.host == b.host
        && a.sandboxes == b.sandboxes
        && left.is_active == right.is_active
        && left.created_at == right.created_at
        && left.last_connected_at == right.last_connected_at
}

fn probe_lists_match(left: &[Probe], right: &[Probe]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(old, new)| probes_match(old, new))
}

fn probe_refresh_changes(
    state: &Ui,
    generation: u64,
    request: u64,
    result: &Result<Vec<Probe>, String>,
) -> bool {
    if state.generation != generation || state.network.borrow().probe_request != request {
        return false;
    }
    match result {
        Ok(probes) => state.probe_error.is_some() || !probe_lists_match(&state.probes, probes),
        Err(error) => state.probe_error.as_ref() != Some(error),
    }
}

fn apply_probe_refresh(
    state: &mut Ui,
    generation: u64,
    request: u64,
    result: Result<Vec<Probe>, String>,
) {
    if !probe_refresh_changes(state, generation, request, &result) {
        return;
    }
    match result {
        Ok(probes) => {
            if !probe_lists_match(&state.probes, &probes) {
                state.probes = probes.into();
            }
            state.probe_error = None;
        }
        Err(error) => state.probe_error = Some(error),
    }
}

/// Split the catalog by kind: agents can only run chat models, while decision
/// models are called from CodeMode and listed separately in settings.
fn apply_models(mut ui: Signal<Ui>, models: Vec<ModelInfo>) {
    let (decision, chat): (Vec<_>, Vec<_>) = models
        .into_iter()
        .partition(|model| model.kind == ModelKind::Decision);

    // Catalog refreshes are infrequent; wire equality includes pricing and all
    // optional metadata without teaching the GUI a second model schema.
    if serde_json::to_vec(&*ui.peek().models).ok() != serde_json::to_vec(&chat).ok() {
        ui.write().models = chat.into();
    }
    if serde_json::to_vec(&*ui.peek().decision_models).ok() != serde_json::to_vec(&decision).ok() {
        ui.write().decision_models = decision.into();
    }
}

pub async fn refresh_metadata(mut ui: Signal<Ui>) {
    let generation = ui.read().generation;
    let Ok(api) = client(ui) else {
        return;
    };
    refresh_probes(ui).await;
    if ui.read().generation != generation {
        return;
    }
    match api.list_models(&ListModels::default()).await {
        Ok(models) if ui.read().generation == generation => apply_models(ui, models),
        Err(_) if ui.read().generation == generation => {
            ui.write().error = Some("Unable to refresh model catalog".into())
        }
        _ => {}
    }
}

fn models_of_kind(state: &Ui, kind: ModelKind) -> &[ModelInfo] {
    match kind {
        ModelKind::Chat => &state.models,
        ModelKind::Decision => &state.decision_models,
    }
}

pub async fn pin_model(mut ui: Signal<Ui>, model: ModelInfo) {
    let generation = ui.read().generation;
    let Ok(api) = client(ui) else {
        return;
    };
    let result = api
        .set_model_pinned(&PinModel {
            model: model.model.clone(),
            pinned: !model.pinned,
        })
        .await;
    if ui.read().generation != generation {
        return;
    }
    match result {
        Ok(_) => {
            // Leave the list alone when a refresh already applied a newer state.
            let current = models_of_kind(&ui.peek(), model.kind)
                .iter()
                .position(|found| found.model == model.model && found.pinned == model.pinned);
            if let Some(index) = current {
                let mut state = ui.write();
                let models = match model.kind {
                    ModelKind::Chat => &mut state.models,
                    ModelKind::Decision => &mut state.decision_models,
                };
                models[index].pinned = !model.pinned;
            }
        }
        Err(_) => ui.write().error = Some("Unable to update server model pin".into()),
    }
}

pub async fn open_session(ui: Signal<Ui>, id: SessionId, permanent: bool) {
    let intent = crate::app::begin_composer_open();
    open_session_with_focus(ui, id, permanent, intent).await;
}

pub(crate) async fn open_session_with_focus(
    ui: Signal<Ui>,
    id: SessionId,
    permanent: bool,
    intent: crate::app::ComposerFocusIntent,
) {
    open_session_selection(ui, id, permanent, false, intent).await;
}

pub async fn double_click_session(ui: Signal<Ui>, id: SessionId) {
    let intent = crate::app::begin_composer_open();
    open_session_selection(ui, id, true, true, intent).await;
}

async fn open_session_selection(
    mut ui: Signal<Ui>,
    id: SessionId,
    permanent: bool,
    from_double_click: bool,
    intent: crate::app::ComposerFocusIntent,
) {
    let request = {
        let state = ui.peek();
        let mut network = state.network.borrow_mut();
        network.selection_request += 1;
        network.selection_request
    };
    let generation = ui.read().generation;
    let Ok(api) = client(ui) else {
        return;
    };
    match api.list_agents(id).await {
        Ok(agents)
            if generation == ui.read().generation
                && request == ui.peek().network.borrow().selection_request =>
        {
            if agents
                .iter()
                .any(|agent| agent.id == crate::app::DRAFT_ROOT)
            {
                ui.write().error = Some("This session contains an unsupported agent ID (0); it cannot be opened safely.".into());
                return;
            }
            let selected = select_session_agent(&agents, ui.read().last_agents.get(&id).copied());
            let rootless = agents.is_empty();
            ui.write().agents.insert(id, agents.into());
            if let Some(agent_id) = selected {
                ui.write().open_session_target_with_focus(
                    TranscriptTarget {
                        session_id: id,
                        agent_id,
                    },
                    permanent,
                    from_double_click,
                    Some(intent),
                );
            } else if rootless {
                ui.write().open_session_target_with_focus(
                    TranscriptTarget {
                        session_id: id,
                        agent_id: crate::app::DRAFT_ROOT,
                    },
                    permanent,
                    from_double_click,
                    Some(intent),
                );
                load_draft_root(ui, id).await;
            } else {
                ui.write().error = Some("This session has no visible root agent".into());
            }
        }
        Err(_) if generation == ui.read().generation => {
            ui.write().error = Some("Unable to load session agents".into())
        }
        _ => {}
    }
}

/// A single session stream is reference-counted by the open views. Target-set
/// changes restart the ordered bootstrap; duplicate panes never add connections.
pub async fn session(ui: Signal<Ui>, session_id: SessionId, targets: Vec<TranscriptTarget>) {
    let mut backoff = ReconnectBackoff::default();
    let generation = ui.read().generation;
    loop {
        if ui.read().generation != generation {
            return;
        }
        let result = session_connection(ui, session_id, &targets).await;
        if ui.peek().generation != generation {
            return;
        }
        if let Err(error) = result {
            set_transcript_status(ui, &targets, disconnected(&error));
            if !can_retry_connection(&error) {
                return;
            }
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

async fn session_connection(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    targets: &[TranscriptTarget],
) -> Result<(), ClientError> {
    let api = client(ui)?;
    let generation = ui.read().generation;
    let known_revision = ui
        .peek()
        .network
        .borrow()
        .revisions
        .get(&session_id)
        .copied();
    let session = api.get_session(session_id).await?;
    if ui.read().generation != generation {
        return Ok(());
    }
    if ui
        .peek()
        .network
        .borrow()
        .revisions
        .get(&session_id)
        .copied()
        == known_revision
        && session_changes(&ui.peek(), &session)
    {
        ui.write().upsert_session(session);
    }
    // Session positions and agent event IDs are distinct. Capture the session
    // head for replay, then bootstrap each agent's bounded history separately.
    let boundary = api
        .list_events(
            session_id,
            None,
            &EventsQuery {
                span: EventSpan::Latest,
                limit: 1,
                ..Default::default()
            },
        )
        .await?;
    if ui.peek().generation != generation {
        return Ok(());
    }
    let mut stream = api
        .stream_events(
            session_id,
            None,
            &EventsQuery {
                span: EventSpan::Forward {
                    after: boundary.head,
                    through: None,
                },
                include_pending: true,
                ..Default::default()
            },
            None,
        )
        .await?;
    if ui.peek().generation != generation {
        return Ok(());
    }
    for target in targets {
        let page = api
            .list_events(
                session_id,
                Some(target.agent_id),
                &EventsQuery {
                    span: EventSpan::Latest,
                    include_pending: true,
                    limit: 150,
                    ..Default::default()
                },
            )
            .await?;
        if ui.peek().generation != generation {
            return Ok(());
        }
        // A context state snapshot may be newer than the page. The subscribed stream
        // buffers all later events, and the projection ignores replay through its head.
        let context_state = api
            .get_agent_context_state(session_id, target.agent_id)
            .await
            .ok();
        if ui.read().generation != generation {
            return Ok(());
        }
        let mut state = ui.write();
        let transcript = state
            .transcripts
            .entry(*target)
            .or_insert_with(|| Transcript::new(session_id, Some(target.agent_id)).into());
        transcript.bootstrap(page);
        transcript.context_state = context_state;
    }
    // The subscribed stream buffers AgentUpdated while the HTTP snapshot loads;
    // consume those updates afterward, so live metadata always wins.
    let agents = api.list_agents(session_id).await?;
    if ui.read().generation != generation {
        return Ok(());
    }
    ui.write().agents.insert(session_id, agents.into());
    set_transcript_status(ui, targets, "Live");
    consume_session_stream(ui, generation, session_id, targets, &mut stream).await
}

fn set_transcript_status(mut ui: Signal<Ui>, targets: &[TranscriptTarget], status: &str) {
    if !targets
        .iter()
        .any(|target| ui.peek().transcript_status.get(target).map(String::as_str) != Some(status))
    {
        return;
    }
    let mut state = ui.write();
    for target in targets {
        if state.transcript_status.get(target).map(String::as_str) != Some(status) {
            state.transcript_status.insert(*target, status.into());
        }
    }
}

const STREAM_FRAME: Duration = Duration::from_millis(16);
const MAX_STREAM_BATCH: usize = 128;

/// Backpressure bounds both queued payloads and reducer work per frame. Only
/// adjacent provisional replacements are compacted; all other ordering is kept.
#[derive(Default)]
struct StreamBatch {
    messages: Vec<StreamMessage>,
    received: usize,
}

impl StreamBatch {
    fn push(&mut self, message: StreamMessage) {
        self.received += 1;
        if let (
            Some(StreamMessage::Upsert { pending: previous }),
            StreamMessage::Upsert { pending },
        ) = (self.messages.last_mut(), &message)
        {
            if previous.session_id == pending.session_id
                && previous.agent_id == pending.agent_id
                && previous.id == pending.id
            {
                if pending.revision > previous.revision {
                    *self.messages.last_mut().unwrap() = message;
                }
                return;
            }
        }
        self.messages.push(message);
    }

    fn is_full(&self) -> bool {
        self.received >= MAX_STREAM_BATCH
    }
}

fn message_target(message: &StreamMessage) -> Option<TranscriptTarget> {
    let (session_id, agent_id) = match message {
        StreamMessage::Upsert { pending } => (pending.session_id, pending.agent_id),
        StreamMessage::Committed { event, .. } => (event.session_id, event.agent_id),
        StreamMessage::Removed {
            session_id,
            agent_id,
            ..
        }
        | StreamMessage::ClientQueryRemoved {
            session_id,
            agent_id,
            ..
        }
        | StreamMessage::RequestFailed {
            session_id,
            agent_id,
            ..
        } => (*session_id, *agent_id),
        StreamMessage::ClientQueryRequested { query } => (query.session_id, query.agent_id),
        StreamMessage::AgentUpdated { agent } => (agent.session_id, agent.id),
        _ => return None,
    };
    Some(TranscriptTarget {
        session_id,
        agent_id,
    })
}

fn message_matches_target(message: &StreamMessage, target: TranscriptTarget) -> bool {
    match message {
        StreamMessage::Snapshot { .. } | StreamMessage::ResyncRequired => true,
        StreamMessage::SessionUpdated { session } => session.id == target.session_id,
        _ => message_target(message) == Some(target),
    }
}

fn agents_match(left: &Agent, right: &Agent) -> bool {
    left.id == right.id
        && left.session_id == right.session_id
        && left.definition_id == right.definition_id
        && left.forked_from == right.forked_from
        && left.subagent_parent_id == right.subagent_parent_id
        && left.name == right.name
        && left.interactive == right.interactive
        && left.hidden == right.hidden
        && left.state == right.state
        && left.usage == right.usage
        && left.next_event_id == right.next_event_id
        && left.created_at == right.created_at
        && left.updated_at == right.updated_at
}

fn agent_metadata_changes(state: &Ui, agent: &Agent) -> bool {
    state
        .agents
        .get(&agent.session_id)
        .and_then(|agents| agents.iter().find(|old| old.id == agent.id))
        .is_none_or(|old| !agents_match(old, agent))
}

fn transcript_may_change(transcript: &Transcript, message: &StreamMessage) -> bool {
    match message {
        StreamMessage::Upsert { pending } => transcript
            .pending
            .get(&pending.id)
            .is_none_or(|old| old.revision < pending.revision),
        StreamMessage::ClientQueryRequested { query } => {
            transcript.queries.get(&query.id) != Some(query)
        }
        StreamMessage::ClientQueryRemoved { query_id, .. } => {
            transcript.queries.contains_key(query_id)
        }
        StreamMessage::AgentUpdated { agent } => transcript
            .agents
            .get(&agent.id)
            .is_none_or(|old| !agents_match(old, agent)),
        StreamMessage::SessionUpdated { session } => transcript
            .session
            .as_ref()
            .is_none_or(|old| !sessions_match(old, session)),
        StreamMessage::RequestFailed {
            request_id,
            message,
            ..
        } => transcript.request_failures.get(request_id) != Some(message),
        // Snapshots and removals can change the private retired-pending set even
        // when visible collections match. Never optimize those barriers away.
        _ => true,
    }
}

fn has_message_destination(
    state: &Ui,
    session_id: SessionId,
    targets: &[TranscriptTarget],
    message: &StreamMessage,
) -> bool {
    let metadata_changes = match message {
        StreamMessage::AgentUpdated { agent } => {
            agent.session_id == session_id && agent_metadata_changes(state, agent)
        }
        StreamMessage::RequestFailed {
            session_id: failed_session,
            request_id,
            message,
            ..
        } => {
            *failed_session == session_id
                && state.error.as_deref()
                    != Some(format!("Request {request_id}: {message}").as_str())
        }
        _ => false,
    };
    metadata_changes
        || targets.iter().any(|target| {
            target.session_id == session_id
                && message_matches_target(message, *target)
                && state
                    .transcripts
                    .get(target)
                    .is_some_and(|transcript| transcript_may_change(transcript, message))
        })
}

fn message_is_immediate(message: &StreamMessage) -> bool {
    match message {
        StreamMessage::Upsert { .. } => false,
        StreamMessage::Committed { event, .. } => matches!(
            event.event.kind,
            psa::api::events::EventType::StatusChange(_)
                | psa::api::events::EventType::InferenceResultStart(_)
                | psa::api::events::EventType::InferenceResultEnd(_)
        ),
        _ => true,
    }
}

fn apply_session_message(
    state: &mut Ui,
    session_id: SessionId,
    targets: &[TranscriptTarget],
    message: StreamMessage,
) {
    match &message {
        StreamMessage::AgentUpdated { agent }
            if agent.session_id == session_id && agent_metadata_changes(state, agent) =>
        {
            let agents = state.agents.entry(agent.session_id).or_default();
            if let Some(old) = agents.iter_mut().find(|a| a.id == agent.id) {
                *old = agent.clone();
            } else {
                agents.push(agent.clone());
            }
        }
        StreamMessage::RequestFailed {
            session_id: failed_session,
            request_id,
            message,
            ..
        } if *failed_session == session_id => {
            state.error = Some(format!("Request {request_id}: {message}"))
        }
        _ => {}
    }
    if let Some(target) = message_target(&message) {
        if target.session_id == session_id
            && targets.contains(&target)
            && state
                .transcripts
                .get(&target)
                .is_some_and(|transcript| transcript_may_change(transcript, &message))
        {
            // Agent-scoped payloads have exactly one projection owner. Move the
            // payload rather than cloning a growing streaming message.
            state.transcripts.get_mut(&target).unwrap().apply(message);
        }
        return;
    }
    // Routing precedes cloning. A session snapshot is authoritative even when
    // this target has no pending items/queries, so empty target slices still apply.
    for target in targets {
        if target.session_id != session_id
            || !message_matches_target(&message, *target)
            || !state
                .transcripts
                .get(target)
                .is_some_and(|transcript| transcript_may_change(transcript, &message))
        {
            continue;
        }
        let Some(transcript) = state.transcripts.get_mut(target) else {
            continue;
        };
        let routed = match &message {
            StreamMessage::Snapshot {
                pending, queries, ..
            } => StreamMessage::Snapshot {
                // Session positions must never replace agent history cursors.
                cursor: transcript.cursor,
                pending: pending
                    .iter()
                    .filter(|item| {
                        item.session_id == target.session_id && item.agent_id == target.agent_id
                    })
                    .cloned()
                    .collect(),
                queries: queries
                    .iter()
                    .filter(|item| {
                        item.session_id == target.session_id && item.agent_id == target.agent_id
                    })
                    .cloned()
                    .collect(),
            },
            _ => message.clone(),
        };
        transcript.apply(routed);
    }
}

fn publish_session_batch(
    mut ui: Signal<Ui>,
    generation: u64,
    session_id: SessionId,
    targets: &[TranscriptTarget],
    batch: &mut StreamBatch,
) -> bool {
    let active = ui.peek().generation == generation;
    let messages = std::mem::take(&mut batch.messages);
    batch.received = 0;
    if !active
        || !messages
            .iter()
            .any(|message| has_message_destination(&ui.peek(), session_id, targets, message))
    {
        return false;
    }
    let mut state = ui.write();
    // Reduce into the current projection, not a cached frame-start clone: older
    // history responses and other panes may have updated it during this frame.
    for message in messages {
        apply_session_message(&mut state, session_id, targets, message);
    }
    true
}

async fn consume_session_stream(
    ui: Signal<Ui>,
    generation: u64,
    session_id: SessionId,
    targets: &[TranscriptTarget],
    stream: &mut EventStream,
) -> Result<(), ClientError> {
    let mut batch = StreamBatch::default();
    let mut deadline = Instant::now() + STREAM_FRAME;
    loop {
        if ui.peek().generation != generation {
            return Ok(());
        }
        let item = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline), if batch.received > 0 => {
                publish_session_batch(ui, generation, session_id, targets, &mut batch);
                deadline = Instant::now() + STREAM_FRAME;
                continue;
            }
            item = stream.next(), if !batch.is_full() => item,
        };
        if ui.peek().generation != generation {
            return Ok(());
        }
        let message = match item {
            Some(Ok(event)) => event.message,
            result => {
                publish_session_batch(ui, generation, session_id, targets, &mut batch);
                return Err(match result {
                    Some(Err(error)) => error,
                    _ => ClientError::Stream("Transcript stream ended".into()),
                });
            }
        };
        if matches!(message, StreamMessage::ResyncRequired) {
            publish_session_batch(ui, generation, session_id, targets, &mut batch);
            return Err(ClientError::Stream("Transcript resync requested".into()));
        }
        if batch.received == 0 {
            deadline = Instant::now() + STREAM_FRAME;
        }
        if !has_message_destination(&ui.peek(), session_id, targets, &message) {
            batch.received += 1;
            continue;
        }
        let immediate = message_is_immediate(&message);
        batch.push(message);
        if immediate {
            publish_session_batch(ui, generation, session_id, targets, &mut batch);
            deadline = Instant::now() + STREAM_FRAME;
        }
    }
}

fn advancing_history_cursor(
    before: u64,
    next: Option<u64>,
    has_older: bool,
) -> (Option<u64>, bool) {
    let next = next.filter(|next| *next < before);
    (next, has_older && next.is_some())
}

fn history_page_makes_progress(has_older: bool, advancing: bool, has_older_event: bool) -> bool {
    !has_older || (advancing && has_older_event)
}

/// Fetch the model context containing an inference: every event from the
/// marker that opened the context through the inference's end.
pub async fn load_context_events(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    turn: u64,
) -> Result<Vec<psa::api::events::Event>, String> {
    const PAGE_LIMIT: u32 = 10_000;
    let generation = ui.peek().generation;
    let api = client(ui).map_err(|_| "Not connected to a server".to_owned())?;

    let mut events = Vec::new();
    let mut after = 0;
    let mut query = EventsQuery {
        span: EventSpan::ContextOf { through: turn },
        limit: PAGE_LIMIT,
        ..Default::default()
    };
    loop {
        let page = api
            .list_events(target.session_id, Some(target.agent_id), &query)
            .await
            .map_err(|error| format!("Unable to load this turn's context: {error}"))?;
        if ui.peek().generation != generation {
            return Err("The server connection changed".into());
        }
        if page.has_more && page.next_after <= after {
            return Err("The server returned a non-progressing page".into());
        }

        events.extend(page.events.into_iter().map(|envelope| envelope.event));
        if !page.has_more {
            return Ok(events);
        }
        after = page.next_after;
        query = EventsQuery {
            span: EventSpan::Forward {
                after,
                through: Some(turn),
            },
            limit: PAGE_LIMIT,
            ..Default::default()
        };
    }
}

/// The model turns on either side of one of an agent's turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdjacentTurns {
    pub turn: u64,
    pub previous: Option<u64>,
    pub next: Option<u64>,
}

fn turn_ends(page: &EventsPage) -> impl Iterator<Item = u64> + '_ {
    page.events
        .iter()
        .filter(|envelope| {
            matches!(
                envelope.event.kind,
                psa::api::events::EventType::InferenceResultEnd(_)
            )
        })
        .map(|envelope| envelope.event.id.0)
}

/// Find a turn's neighbors by paging outward through the agent's history.
pub async fn load_adjacent_turns(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    turn: u64,
) -> Result<AdjacentTurns, String> {
    const PAGE_LIMIT: u32 = 200;
    let api = client(ui).map_err(|_| "Not connected to a server".to_owned())?;
    let describe = |error| format!("Unable to find the adjacent turns: {error}");

    let mut before = turn;
    let previous = loop {
        let query = EventsQuery {
            span: EventSpan::Before(before),
            limit: PAGE_LIMIT,
            ..Default::default()
        };
        let page = api
            .list_events(target.session_id, Some(target.agent_id), &query)
            .await
            .map_err(describe)?;
        if let Some(found) = turn_ends(&page).max() {
            break Some(found);
        }
        match page.next_before {
            Some(next) if page.has_older && next < before => before = next,
            _ => break None,
        }
    };

    let mut after = turn;
    let next = loop {
        let query = EventsQuery {
            span: EventSpan::Forward {
                after,
                through: None,
            },
            limit: PAGE_LIMIT,
            ..Default::default()
        };
        let page = api
            .list_events(target.session_id, Some(target.agent_id), &query)
            .await
            .map_err(describe)?;
        if let Some(found) = turn_ends(&page).min() {
            break Some(found);
        }
        if !page.has_more || page.next_after <= after {
            break None;
        }
        after = page.next_after;
    };

    Ok(AdjacentTurns {
        turn,
        previous,
        next,
    })
}

pub async fn older(mut ui: Signal<Ui>, target: TranscriptTarget) -> bool {
    let generation = ui.read().generation;
    let cursor = ui
        .read()
        .transcripts
        .get(&target)
        .and_then(|t| t.next_before);
    let Some(before) = cursor else {
        ui.write().error = Some("Unable to load older history: missing history cursor".into());
        return false;
    };
    let Ok(api) = client(ui) else {
        ui.write().error = Some("Unable to load older history: not connected".into());
        return false;
    };
    match api
        .list_events(
            target.session_id,
            Some(target.agent_id),
            &EventsQuery {
                span: EventSpan::Before(before),
                limit: 150,
                ..Default::default()
            },
        )
        .await
    {
        Ok(page) if generation == ui.read().generation => {
            let (next, has_older) =
                advancing_history_cursor(before, page.next_before, page.has_older);
            if !history_page_makes_progress(
                page.has_older,
                has_older,
                page.events.iter().any(|event| {
                    event.session_id == target.session_id
                        && event.agent_id == target.agent_id
                        && event.event.id.0 < before
                }),
            ) {
                ui.write().error = Some(
                    "Unable to load older history: the server returned a non-progressing page"
                        .into(),
                );
                return false;
            }
            if let Some(t) = ui.write().transcripts.get_mut(&target) {
                // Ignore a response for a cursor superseded by another pane or resync.
                if t.next_before == Some(before) {
                    t.merge_history(page);
                    // Empty pages may not update the projection's boundary. Always
                    // advance it, and stop on malformed/non-progressing cursors.
                    if t.next_before == Some(before) || !has_older {
                        t.next_before = next;
                        t.has_older = has_older;
                    }
                    return true;
                }
            }
            false
        }
        Err(_) if generation == ui.read().generation => {
            ui.write().error = Some("Unable to load older history".into());
            false
        }
        _ => false,
    }
}

struct CapturedDraft {
    text: String,
    revision: u64,
    generation: u64,
}

fn capture_draft(ui: Signal<Ui>, target: TranscriptTarget) -> CapturedDraft {
    let state = ui.peek();
    CapturedDraft {
        text: state.drafts.get(&target).cloned().unwrap_or_default(),
        revision: state.drafts.revision(&target),
        generation: state.generation,
    }
}

/// Capture at the user action, before a spawned future can observe another edit.
pub fn submit(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    interrupt: bool,
) -> impl std::future::Future<Output = ()> {
    let draft = capture_draft(ui, target);
    submit_captured(ui, target, interrupt, draft)
}

/// `/fork [name]`: copies the session through its current head, then opens the
/// fork at the copy of the agent the command was typed in.
pub async fn fork_session(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    name: String,
) -> CommandEnd {
    let generation = ui.peek().generation;
    let Ok(api) = client(ui) else {
        return None;
    };
    let request = ForkSession {
        point: ForkPoint::Head,
        metadata: UpdateSession {
            name: Some(name).filter(|name| !name.is_empty()),
            ..Default::default()
        },
    };
    let result = api.fork_session(target.session_id, &request).await;
    if ui.peek().generation != generation {
        return None;
    }
    let fork = match result {
        Ok(fork) => fork,
        Err(error) => {
            let message = format!("Unable to fork the session: {error}");
            ui.write().error = Some(message.clone());
            return Some(Err(message));
        }
    };

    let id = fork.session.id;
    {
        let mut state = ui.write();
        if let Some(agent) = fork
            .agents
            .iter()
            .find(|agent| agent.forked_from == Some(target.agent_id))
        {
            state.last_agents.insert(id, agent.id);
        }
        state.upsert_session(fork.session);
        state.notice = Some("Session forked.".into());
    }
    open_session(ui, id, true).await;
    Some(Ok(()))
}

/// Opens a session that may not be in the loaded list, such as a fork's parent,
/// loading its metadata first so its details render immediately.
pub async fn open_session_by_id(mut ui: Signal<Ui>, id: SessionId) {
    let generation = ui.peek().generation;
    let Ok(api) = client(ui) else {
        return;
    };
    match api.get_session(id).await {
        Ok(session) if ui.peek().generation == generation => ui.write().upsert_session(session),
        Ok(_) => return,
        Err(error) => {
            ui.write().error = Some(format!("Unable to open session {}: {error}", id.0));
            return;
        }
    }
    open_session(ui, id, true).await;
}

/// `/refresh` outside a new session: rescans the global and the session's
/// `.agents` agent and skill configuration. The server's `config.toml` still requires a restart.
pub async fn refresh_configs(mut ui: Signal<Ui>, session_id: SessionId) -> CommandEnd {
    let generation = ui.peek().generation;
    let result = match client(ui) {
        Ok(api) => match api.refresh_configs(&[session_id]).await {
            Ok(()) => api.list_agent_configs().await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if ui.peek().generation != generation {
        return None;
    }
    let mut state = ui.write();
    match result {
        Ok(items) => {
            let message = format!("Reloaded {} agent configurations.", items.len());
            state.notice = Some(message.clone());
            Some(Ok(()))
        }
        Err(error) => {
            let message = format!("Unable to refresh configurations: {error}");
            state.error = Some(message.clone());
            Some(Err(message))
        }
    }
}

/// How a client-side slash command ended: its failure message on error, or
/// `None` when the UI moved on (such as a reconnect) and nothing is recorded.
pub type CommandEnd = Option<Result<(), String>>;

/// Run a client-side slash command, then record it in the transcript of the
/// agent it was typed to. Root drafts have no agent yet, so nothing is recorded.
pub async fn record_command(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    id: client_shared::CommandId,
    args: String,
    action: impl std::future::Future<Output = CommandEnd>,
) {
    let Some(ended) = action.await else {
        return;
    };
    if target.agent_id == crate::app::DRAFT_ROOT {
        return;
    }
    let Some(record) = client_shared::build_command_record(id, &args, ended) else {
        return;
    };
    let Ok(api) = client(ui) else {
        return;
    };
    let name = id.spec().name;
    if let Err(error) = api
        .record_command(target.session_id, target.agent_id, record)
        .await
    {
        let mut state = ui.write();
        if state.error.is_none() {
            state.error = Some(format!("Unable to record /{name}: {error}"));
        }
    }
}

/// Steers `/compact` to an existing agent and waits for it to finish; the
/// server records it in the transcript.
pub fn submit_compact(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    instructions: String,
) -> impl std::future::Future<Output = ()> {
    let instructions = Some(instructions).filter(|text| !text.is_empty());
    let sent = send_agent_command(ui, target, AgentCommand::Compact { instructions });
    async move {
        let Some(result) = sent.await else {
            return;
        };
        let mut ui = ui;
        let mut state = ui.write();
        match result.outcome {
            CommandOutcome::Succeeded => state.notice = Some("Compaction finished.".into()),
            CommandOutcome::Interrupted => state.notice = Some("Compaction interrupted.".into()),
            CommandOutcome::Failed => {
                state.error = Some(result.output.unwrap_or_else(|| "Compaction failed.".into()))
            }
        }
    }
}

/// Steers `/goal` to an existing agent. Steering lets pause and clear reach a
/// running goal, which would otherwise keep its run going indefinitely.
pub fn submit_goal(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    arguments: String,
) -> impl std::future::Future<Output = ()> {
    let command = AgentCommand::Goal(GoalCommand::parse(&arguments));
    let sent = send_agent_command(ui, target, command);
    async move {
        let Some(result) = sent.await else {
            return;
        };
        let mut ui = ui;
        let mut state = ui.write();
        match result.outcome {
            CommandOutcome::Succeeded => state.notice = result.output,
            CommandOutcome::Interrupted => state.notice = Some("Goal change interrupted.".into()),
            CommandOutcome::Failed => {
                state.error = Some(
                    result
                        .output
                        .unwrap_or_else(|| "Goal change failed.".into()),
                )
            }
        }
    }
}

/// Steers an agent command and waits for its result. The draft clears only if
/// unchanged since the command was captured; rejected commands keep it for
/// editing. `None` when the command could not be sent or the UI moved on.
fn send_agent_command(
    ui: Signal<Ui>,
    target: TranscriptTarget,
    command: AgentCommand,
) -> impl std::future::Future<Output = Option<CommandResult>> {
    let draft = capture_draft(ui, target);
    async move {
        let api = client(ui).ok()?;
        let name = command.name();
        let result = api
            .send_command(
                target.session_id,
                target.agent_id,
                CommandDelivery::Steer,
                command,
            )
            .await;
        if ui.peek().generation != draft.generation {
            return None;
        }
        let mut ui = ui;
        let mut state = ui.write();
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                state.error = Some(format!("Unable to run /{name}: {error}"));
                return None;
            }
        };
        state.drafts.clear_if_revision(&target, draft.revision);
        Some(result)
    }
}

async fn submit_captured(
    mut ui: Signal<Ui>,
    target: TranscriptTarget,
    interrupt: bool,
    draft: CapturedDraft,
) {
    let CapturedDraft {
        text,
        revision: draft_revision,
        generation,
    } = draft;
    if ui.peek().generation != generation
        || (interrupt && target.agent_id == crate::app::DRAFT_ROOT)
    {
        return;
    }
    if !interrupt && (text.trim().is_empty() || ui.read().uncertain_submissions.contains(&target)) {
        return;
    }
    let parsed = parse_composer_input(&text, Frontend::Gui);
    if !interrupt && matches!(parsed, ComposerInput::Command { .. }) {
        ui.write().error = Some(
            "Choose a session default from /model, or use // to send literal slash text.".into(),
        );
        return;
    }
    let content = match parsed {
        ComposerInput::Text(text) => text,
        ComposerInput::Command { .. } => String::new(),
    };
    let request = if interrupt {
        AgentRequest::Interrupt
    } else {
        AgentRequest::Message {
            source: None,
            delivery: MessageDelivery::Steer,
            content: psa::api::events::Message::text(psa::api::events::MessageType::Human, content)
                .content,
        }
    };
    let Ok(api) = client(ui) else {
        return;
    };
    let inserted = if interrupt {
        ui.write().pending_interrupts.insert(target)
    } else {
        ui.write().pending_submissions.insert(target)
    };
    if !inserted {
        return;
    }
    let result = api
        .submit_request(target.session_id, target.agent_id, &request)
        .await;
    if generation != ui.read().generation {
        return;
    }
    if interrupt {
        ui.write().pending_interrupts.remove(&target);
    } else {
        ui.write().pending_submissions.remove(&target);
    }
    match result {
        Ok(accepted) => {
            let mut state = ui.write();
            if !interrupt {
                state.drafts.clear_if_revision(&target, draft_revision);
            }
            state.notice = Some(format!(
                "Request {} accepted; delivery pending",
                accepted.request_id
            ));
        }
        Err(_) => {
            let mut state = ui.write();
            if !interrupt {
                state.uncertain_submissions.insert(target);
            }
            state.error = Some(if interrupt {
                "Interrupt request failed; delivery may be unknown. Check the agent status before retrying.".into()
            } else {
                "Submission failed; delivery may be unknown. Draft retained; check the transcript before explicitly enabling retry.".into()
            });
        }
    }
}

/// Load the draft's choices without creating an agent.
pub async fn load_draft_root(mut ui: Signal<Ui>, session_id: SessionId) {
    if ui
        .peek()
        .draft_roots
        .get(&session_id)
        .is_some_and(|draft| draft.loading || draft.ready)
    {
        return;
    }
    ui.write()
        .draft_roots
        .entry(session_id)
        .or_default()
        .loading = true;
    let generation = ui.read().generation;
    let result = async {
        let api = client(ui)?;
        let config = api.get_config().await?;
        api.refresh_configs(&[session_id]).await?;
        let items = api.list_agent_configs().await?;
        Ok::<_, ClientError>((config, items))
    }
    .await;
    if ui.read().generation != generation {
        return;
    }
    let mut state = ui.write();
    let draft = state.draft_roots.entry(session_id).or_default();
    draft.loading = false;
    match result {
        Ok((config, items)) => {
            draft.apply_configs(items, &config.default_agent_name);
            draft.ready = true;
        }
        Err(_) => {
            state.error = Some("Unable to load agent configurations; refresh to retry.".into())
        }
    }
}

fn promote_draft_root(
    state: &mut Ui,
    session_id: SessionId,
    agent: Agent,
) -> Option<TranscriptTarget> {
    let from = TranscriptTarget {
        session_id,
        agent_id: crate::app::DRAFT_ROOT,
    };
    let to = TranscriptTarget {
        session_id,
        agent_id: agent.id,
    };
    if !state.drafts.can_transfer(&from, &to) || !state.drafts.transfer(&from, to) {
        let draft = state.draft_roots.entry(session_id).or_default();
        draft.pending = false;
        draft.uncertain = true;
        state.notice = Some("Existing agent already has an editor/draft; first message retained here. Copy or merge it into the existing agent's editor before sending.".into());
        return None;
    }
    state.workspace.replace_target(from, to);
    state.last_agents.insert(session_id, agent.id);
    let agents = state.agents.entry(session_id).or_default();
    if !agents.iter().any(|a| a.id == agent.id) {
        agents.push(agent);
    }
    state.draft_roots.remove(&session_id);
    Some(to)
}

struct CapturedRootSubmission {
    draft: CapturedDraft,
    config: AgentConfig,
    model: ModelSelection,
}

fn require_root_selection(
    draft: &crate::app::DraftRoot,
) -> Result<(AgentConfig, ModelSelection), &'static str> {
    let config = draft
        .configs
        .iter()
        .find(|config| Some(&config.config.config_id) == draft.selected.as_ref())
        .ok_or("Select an available agent configuration first using /select agent.")?;
    let model = draft
        .model
        .clone()
        .filter(|model| config.config.models.contains(model))
        .ok_or(
            "Choose an allowed model policy using /select agent before sending the first message.",
        )?;
    Ok((config.config.clone(), model))
}

fn capture_root_submission(
    mut ui: Signal<Ui>,
    session_id: SessionId,
) -> Option<CapturedRootSubmission> {
    let draft = capture_draft(
        ui,
        TranscriptTarget {
            session_id,
            agent_id: crate::app::DRAFT_ROOT,
        },
    );
    if draft.text.trim().is_empty()
        || !matches!(
            parse_composer_input(&draft.text, Frontend::Gui),
            ComposerInput::Text(_)
        )
    {
        return None;
    }
    let selection = {
        let state = ui.peek();
        let root = state.draft_roots.get(&session_id)?;
        if root.pending || root.uncertain {
            return None;
        }
        require_root_selection(root)
    };
    let (config, model) = match selection {
        Ok(selection) => selection,
        Err(error) => {
            ui.write().error = Some(error.into());
            return None;
        }
    };
    // Deduplicate actions immediately, including futures not polled yet.
    ui.write().draft_roots.get_mut(&session_id).unwrap().pending = true;
    Some(CapturedRootSubmission {
        draft,
        config,
        model,
    })
}

pub fn submit_draft_root(
    ui: Signal<Ui>,
    session_id: SessionId,
) -> impl std::future::Future<Output = ()> {
    let captured = capture_root_submission(ui, session_id);
    async move {
        if let Some(captured) = captured {
            submit_captured_root(ui, session_id, captured).await;
        }
    }
}

async fn submit_captured_root(
    mut ui: Signal<Ui>,
    session_id: SessionId,
    captured: CapturedRootSubmission,
) {
    let CapturedRootSubmission {
        draft: captured_draft,
        config,
        model,
    } = captured;
    let generation = captured_draft.generation;
    if ui.peek().generation != generation {
        return;
    }
    let Ok(api) = client(ui) else {
        ui.write().draft_roots.get_mut(&session_id).unwrap().pending = false;
        return;
    };
    // Reconcile with persisted state before issuing a create, including sessions
    // opened by another client since this draft was displayed.
    let agents = api.list_agents(session_id).await;
    if ui.read().generation != generation {
        return;
    }
    let agents = match agents {
        Ok(agents) => agents,
        Err(_) => {
            ui.write().draft_roots.get_mut(&session_id).unwrap().pending = false;
            ui.write().error =
                Some("Unable to check for an existing root agent; no agent was created.".into());
            return;
        }
    };
    if agents
        .iter()
        .any(|agent| agent.id == crate::app::DRAFT_ROOT)
    {
        ui.write().draft_roots.get_mut(&session_id).unwrap().pending = false;
        ui.write().error =
            Some("This session contains an unsupported agent ID (0); no agent was created.".into());
        return;
    }
    if !agents.is_empty() {
        if let Some(id) = select_session_agent(&agents, None) {
            let agent = agents.into_iter().find(|a| a.id == id).unwrap();
            if promote_draft_root(&mut ui.write(), session_id, agent).is_some() {
                ui.write().notice = Some(
                    "An agent already exists. Review the retained draft before sending.".into(),
                );
            }
        } else {
            ui.write().draft_roots.get_mut(&session_id).unwrap().pending = false;
            ui.write().error = Some("This session already has agents but no visible root.".into());
        }
        return;
    }
    let request = CreateAgent {
        name: config.config_id.name.clone(),
        config_id: config.config_id,
        subagent_parent_id: None,
        interactive: true,
        hidden: false,
        model: Some(model),
    };
    let result = api.create_agent(session_id, &request).await;
    if ui.read().generation != generation {
        return;
    }
    match result {
        Ok(agent) if agent.id == crate::app::DRAFT_ROOT => {
            let mut state = ui.write();
            let draft = state.draft_roots.get_mut(&session_id).unwrap();
            draft.pending = false;
            draft.uncertain = true;
            state.error = Some(
                "The created agent has unsupported ID 0. The message was not sent; draft retained."
                    .into(),
            );
        }
        Ok(agent) => {
            let promoted = promote_draft_root(&mut ui.write(), session_id, agent);
            if let Some(real) = promoted {
                submit_captured(ui, real, false, captured_draft).await;
            }
        }
        Err(_) => {
            let mut state = ui.write();
            let draft = state.draft_roots.get_mut(&session_id).unwrap();
            draft.pending = false;
            draft.uncertain = true;
            state.error = Some("Agent creation failed or delivery is uncertain. Draft retained. Reopen the session to check for an existing agent before retrying.".into());
        }
    }
}

#[cfg(test)]
#[path = "tests/network/performance_tests/mod.rs"]
mod performance_tests;

#[cfg(test)]
mod history_tests {
    use super::{advancing_history_cursor, history_page_makes_progress};

    #[test]
    fn cursor_must_advance_and_server_must_report_more() {
        assert_eq!(
            advancing_history_cursor(20, Some(10), true),
            (Some(10), true)
        );
        assert_eq!(
            advancing_history_cursor(20, Some(10), false),
            (Some(10), false)
        );
        assert_eq!(advancing_history_cursor(20, Some(20), true), (None, false));
        assert_eq!(advancing_history_cursor(20, Some(21), true), (None, false));
        assert_eq!(advancing_history_cursor(20, None, true), (None, false));
        assert!(history_page_makes_progress(false, false, false));
        assert!(history_page_makes_progress(true, true, true));
        assert!(!history_page_makes_progress(true, false, true));
        assert!(!history_page_makes_progress(true, true, false));
    }
}

#[cfg(test)]
mod adjacent_turn_tests {
    use super::*;
    use psa::api::events::{EventType, InferenceResultEnd, Message, MessageType};
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn empty_root() -> Element {
        rsx! { div {} }
    }

    fn envelope(id: u64, kind: EventType) -> serde_json::Value {
        serde_json::json!({
            "session_id": 1, "agent_id": 2, "position": id,
            "event": { "id": id, "source_agent_id": 2, "timestamp": "2026-01-01T00:00:00Z", "kind": kind },
        })
    }

    fn end(id: u64) -> serde_json::Value {
        envelope(
            id,
            EventType::InferenceResultEnd(InferenceResultEnd {
                id,
                outcome: llm_api::api::Outcome::Cancelled,
                usage: Default::default(),
                request_time: std::time::Duration::ZERO,
                response_id: None,
                provider_metadata: Default::default(),
            }),
        )
    }

    fn other(id: u64) -> serde_json::Value {
        envelope(
            id,
            EventType::Message(Message::text(MessageType::Agent, "text")),
        )
    }

    fn page(
        events: Vec<serde_json::Value>,
        next_before: Option<u64>,
        next_after: u64,
        has_more: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "events": events, "pending": [], "queries": [], "next_after": next_after, "head": 20,
            "has_more": has_more, "next_before": next_before, "has_older": next_before.is_some(),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn neighbors_are_found_across_pages_and_absent_at_the_ends() {
        let responses = [
            ("before=10", page(vec![other(9)], Some(9), 0, false)),
            ("before=9", page(vec![end(5), end(7)], None, 0, false)),
            ("after=10", page(vec![other(11)], None, 11, true)),
            ("after=11", page(vec![end(12), end(14)], None, 14, false)),
            ("before=5", page(vec![], None, 0, false)),
            ("after=5", page(vec![end(7)], None, 7, false)),
            ("before=14", page(vec![end(12)], None, 0, false)),
            ("after=14", page(vec![other(15)], None, 15, false)),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (expected, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data = Vec::new();
                let mut buffer = [0; 4096];
                while !data.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    data.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8_lossy(&data);
                let line = request.lines().next().unwrap();
                assert!(line.contains(expected), "{line} lacks {expected}");
                let body = body.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });

        let mut dom = VirtualDom::new(empty_root);
        dom.rebuild_in_place();
        let mut settings = DesktopSettings::default();
        settings.active_profile_mut().connection.api_url = format!("http://{address}");
        let ui = dom.in_scope(ScopeId::ROOT, || Signal::new(Ui::new(settings)));
        let target = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(2),
        };

        let found = dom.in_runtime(|| {
            Box::pin(async move {
                let middle = load_adjacent_turns(ui, target, 10).await.unwrap();
                let first = load_adjacent_turns(ui, target, 5).await.unwrap();
                let last = load_adjacent_turns(ui, target, 14).await.unwrap();
                (middle, first, last)
            })
        });
        let (middle, first, last) = found.await;
        assert_eq!((middle.previous, middle.next), (Some(7), Some(12)));
        assert_eq!((first.previous, first.next), (None, Some(7)));
        assert_eq!((last.previous, last.next), (Some(12), None));
        server.join().unwrap();
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    fn empty_root() -> Element {
        rsx! { div {} }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_is_deduplicated_preserves_draft_and_ignores_stale_profile() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buffer = [0; 4096];
                let (body_start, length) = loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buffer[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break (end + 4, length);
                        }
                    }
                };
                requests.push((
                    String::from_utf8_lossy(&data[..body_start])
                        .lines()
                        .next()
                        .unwrap()
                        .to_owned(),
                    String::from_utf8(data[body_start..body_start + length].to_vec()).unwrap(),
                ));
                std::thread::sleep(Duration::from_millis(100));
                let body = r#"{"request_id":"accepted"}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let mut dom = VirtualDom::new(empty_root);
        dom.rebuild_in_place();
        let mut settings = DesktopSettings::default();
        settings.active_profile_mut().connection.api_url = format!("http://{address}");
        let mut ui = dom.in_scope(ScopeId::ROOT, || Signal::new(Ui::new(settings)));
        let target = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: AgentId(42),
        };
        let mut run = Box::pin(async move {
            ui.write().drafts.insert(target, "unsent draft".into());
            futures_util::join!(submit(ui, target, true), submit(ui, target, true));
            assert_eq!(
                ui.read().drafts.get(&target).map(String::as_str),
                Some("unsent draft")
            );
            assert!(ui.read().pending_interrupts.is_empty());
            assert!(ui.read().notice.as_deref().unwrap().contains("accepted"));
            let draft = TranscriptTarget {
                agent_id: crate::app::DRAFT_ROOT,
                ..target
            };
            submit(ui, draft, true).await;
            assert!(ui.read().pending_interrupts.is_empty());

            let change = async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert!(ui.read().pending_interrupts.contains(&target));
                ui.write()
                    .change_connection("http://127.0.0.1:1".into(), String::new());
            };
            futures_util::join!(submit(ui, target, true), change);
            assert!(ui.read().notice.is_none());
            assert!(ui.read().error.is_none());
            assert!(ui.read().pending_interrupts.is_empty());
        });
        std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        for (line, body) in requests {
            assert!(
                line.starts_with("POST /sessions/1/agents/42/requests "),
                "{line}"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap(),
                "Interrupt"
            );
        }
    }
}

#[cfg(test)]
mod probe_refresh_tests {
    use super::*;

    #[test]
    fn stale_probe_responses_cannot_override_newer_requests_or_profiles() {
        let mut ui = Ui::new(DesktopSettings::default());
        ui.network.borrow_mut().probe_request = 2;
        apply_probe_refresh(&mut ui, 0, 2, Err("new failure".into()));
        apply_probe_refresh(&mut ui, 0, 1, Ok(Vec::new()));
        assert_eq!(ui.probe_error.as_deref(), Some("new failure"));
        ui.generation = 1;
        apply_probe_refresh(&mut ui, 0, 2, Ok(Vec::new()));
        assert_eq!(ui.probe_error.as_deref(), Some("new failure"));
        apply_probe_refresh(&mut ui, 1, 2, Ok(Vec::new()));
        assert!(ui.probe_error.is_none());
    }
}

#[cfg(test)]
mod draft_root_tests {
    use super::*;
    fn empty_root() -> Element {
        rsx! { div {} }
    }

    // Use a real HTTP transport and the GUI's actual draft creation/submission
    // path, while keeping the server deterministic and independent of probes.
    #[tokio::test(flavor = "current_thread")]
    async fn first_message_selects_config_and_retains_uncertain_submission() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::{Arc, Mutex},
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let recorded = requests.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..7 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                let (header_end, length) = loop {
                    let n = stream.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buffer[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + len {
                            break (end + 4, len);
                        }
                    }
                };
                let headers = String::from_utf8_lossy(&data[..header_end]);
                let line = headers.lines().next().unwrap().to_owned();
                let body =
                    String::from_utf8(data[header_end..header_end + length].to_vec()).unwrap();
                recorded.lock().unwrap().push((line.clone(), body));
                let (status, response) = if line.starts_with("GET /sessions/1/agents ") {
                    ("200 OK", serde_json::json!([]))
                } else if line.starts_with("GET /config ") {
                    (
                        "200 OK",
                        serde_json::json!({"default_agent_name":"default"}),
                    )
                } else if line.starts_with("POST /configs/refresh ") {
                    ("204 No Content", serde_json::Value::Null)
                } else if line.starts_with("GET /agent-configs") {
                    (
                        "200 OK",
                        serde_json::json!([
                            {"config":{"config_id":{"config_group":"Global","name":"default"},"selectable":true,"models":["Global"]},"status":{"missing_components":[]}},
                            {"config":{"config_id":{"config_group":"Global","name":"alternate"},"selectable":true,"models":["Global"]},"status":{"missing_components":[]}}
                        ]),
                    )
                } else if line.starts_with("POST /sessions/1/agents ") {
                    (
                        "200 OK",
                        serde_json::json!({"id":42,"definition_id":1,"session_id":1,"forked_from":null,"subagent_parent_id":null,"name":"alternate","hidden":false,"interactive":true,"state":"Idle","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0},"next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}),
                    )
                } else if line.starts_with("POST /sessions/1/agents/42/requests ") {
                    ("500 Internal Server Error", serde_json::json!({}))
                } else {
                    panic!("unexpected request: {line}")
                };
                let body = if response.is_null() {
                    String::new()
                } else {
                    response.to_string()
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut dom = VirtualDom::new(empty_root);
        dom.rebuild_in_place();
        let mut settings = DesktopSettings::default();
        settings.active_profile_mut().connection.api_url = format!("http://{address}");
        let mut ui = dom.in_scope(ScopeId::ROOT, || Signal::new(Ui::new(settings)));
        let mut run = Box::pin(async move {
            open_session(ui, SessionId(1), true).await;
            assert_eq!(requests.lock().unwrap().len(), 4);
            // Opening only reads, apart from rescanning the session's configs.
            assert!(requests.lock().unwrap().iter().all(|(line, _)| {
                line.starts_with("GET ") || line.starts_with("POST /configs/refresh ")
            }));
            let target = TranscriptTarget {
                session_id: SessionId(1),
                agent_id: crate::app::DRAFT_ROOT,
            };
            ui.write()
                .drafts
                .insert(target, "/select agent alternate".into());
            submit_draft_root(ui, SessionId(1)).await;
            assert_eq!(
                requests.lock().unwrap().len(),
                4,
                "slash command must not create an agent"
            );
            {
                let mut state = ui.write();
                let draft = state.draft_roots.get_mut(&SessionId(1)).unwrap();
                let id = draft
                    .configs
                    .iter()
                    .find(|c| c.config.config_id.name == "alternate")
                    .unwrap()
                    .config
                    .config_id
                    .clone();
                draft.select_config(id);
                state.drafts.insert(target, "first message".into());
            }
            submit_draft_root(ui, SessionId(1)).await;
            let real = TranscriptTarget {
                session_id: SessionId(1),
                agent_id: AgentId(42),
            };
            assert_eq!(
                ui.read().drafts.get(&real).map(String::as_str),
                Some("first message")
            );
            assert!(ui.read().uncertain_submissions.contains(&real));
            submit(ui, real, false).await;
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 7);
            let create: serde_json::Value = serde_json::from_str(&requests[5].1).unwrap();
            assert_eq!(create["config_id"]["name"], "alternate");
            assert!(requests[6].1.contains("first message"));
        });
        std::future::poll_fn(|cx| dom.in_scope(ScopeId::ROOT, || run.as_mut().poll(cx))).await;
        server.join().unwrap();
    }

    #[test]
    fn required_model_policy_is_not_implicitly_selected() {
        let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
            "config": {"config_id":{"config_group":"Global","name":"root"}, "selectable":true, "models":["Global",{"Fixed":{"model":"openrouter/vendor/model","effort":"Global"}}], "default":false},
            "status": {"missing_components":[]}
        })).unwrap();
        let id = status.config.config_id.clone();
        let mut draft = crate::app::DraftRoot::default();
        draft.configs.push(status);
        draft.select_config(id.clone());
        assert!(draft.model.is_none());
        draft.configs[0].config.first_model_default = true;
        draft.select_config(id);
        assert_eq!(draft.model, Some(ModelSelection::Global));
    }

    #[test]
    fn refresh_preserves_valid_root_selection_and_policy() {
        let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
            "config": {"config_id":{"config_group":"Global","name":"root"}, "selectable":true, "models":["Global",{"Fixed":{"model":"openrouter/vendor/model","effort":"Global"}}], "default":false},
            "status": {"missing_components":[]}
        })).unwrap();
        let id = status.config.config_id.clone();
        let policy = status.config.models[1].clone();
        let mut draft = crate::app::DraftRoot::default();
        draft.configs.push(status.clone());
        draft.select_config(id.clone());
        draft.model = Some(policy.clone());
        draft.apply_configs(vec![status.clone()], "other");
        assert_eq!(draft.selected, Some(id.clone()));
        assert_eq!(draft.model, Some(policy));
        let mut changed = status;
        changed.config.models.truncate(1);
        draft.apply_configs(vec![changed], "other");
        assert_eq!(draft.selected, Some(id));
        assert_eq!(draft.model, Some(ModelSelection::Global));
        draft.apply_configs(vec![], "other");
        assert_eq!(draft.selected, None);
        assert_eq!(draft.model, None);
    }

    #[test]
    fn root_model_choices_keep_first_allowed_wire_selector() {
        let status: AgentConfigStatus = serde_json::from_value(serde_json::json!({
            "config": {"config_id":{"config_group":"Global","name":"root"}, "models":[
                "Parent", "Global",
                {"Fixed":{"model":"openrouter/vendor/model","effort":"Parent"}},
                {"Fixed":{"model":"openrouter/vendor/model","effort":"Global"}},
                {"Fixed":{"model":"openrouter/vendor/other","effort":"Global"}}
            ], "default":false},
            "status": {"missing_components":[]}
        }))
        .unwrap();
        let id = status.config.config_id.clone();
        let mut draft = crate::app::DraftRoot::default();
        draft.configs.push(status);
        draft.select_config(id.clone());
        let choices = draft.model_choices();
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0], ModelSelection::Parent);
        assert_eq!(choices[1], draft.configs[0].config.models[2]);
        assert_eq!(choices[2], draft.configs[0].config.models[4]);
        assert!(draft.model.is_none());

        draft.configs[0].config.models.truncate(2);
        draft.select_config(id);
        assert_eq!(draft.model_choices(), vec![ModelSelection::Parent]);
        assert_eq!(draft.model, Some(ModelSelection::Parent));
    }

    #[test]
    fn promotion_preserves_draft_and_tab() {
        let mut ui = Ui::new(DesktopSettings::default());
        let from = TranscriptTarget {
            session_id: SessionId(1),
            agent_id: crate::app::DRAFT_ROOT,
        };
        ui.open(from, true);
        let tab = ui.workspace.groups()[0].tabs[0].id;
        ui.drafts.insert(from, "first message".into());
        let agent: Agent = serde_json::from_value(serde_json::json!({"id":42,"definition_id":1,"session_id":1,"forked_from":null,"subagent_parent_id":null,"name":"root","hidden":false,"interactive":true,"state":"Idle","usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0,"cached_input_tokens":0,"reasoning_output_tokens":0},"next_event_id":1,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"})).unwrap();
        let to = promote_draft_root(&mut ui, SessionId(1), agent).unwrap();
        assert_eq!(ui.workspace.groups()[0].tabs[0].id, tab);
        assert_eq!(ui.workspace.groups()[0].tabs[0].target, to);
        assert_eq!(
            ui.drafts.get(&to).map(String::as_str),
            Some("first message")
        );
        assert!(!ui.drafts.contains_key(&from));
    }
}
