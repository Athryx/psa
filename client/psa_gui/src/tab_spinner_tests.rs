use super::*;

fn agent(id: u64, session: u64, state: AgentState) -> Agent {
    let mut agent: Agent = serde_json::from_value(serde_json::json!({
        "id": id, "definition_id": 1, "session_id": session,
        "parent_id": null, "name": format!("agent-{id}"), "interactive": true,
        "state": "Idle", "usage": {"input_tokens": 0, "output_tokens": 0,
            "total_tokens": 0, "cached_input_tokens": 0, "reasoning_output_tokens": 0},
        "next_event_id": 1, "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    agent.state = state;
    agent
}

fn target(session: u64, agent: u64) -> TranscriptTarget {
    TranscriptTarget {
        session_id: SessionId(session),
        agent_id: AgentId(agent),
    }
}

fn fixture() -> Ui {
    let mut state = Ui::new(DesktopSettings::default());
    state.connection = "Connected".into();
    state.agents.insert(
        SessionId(1),
        vec![
            agent(1, 1, AgentState::Running),
            agent(2, 1, AgentState::Idle),
            agent(0, 1, AgentState::Running),
        ],
    );
    for target in [target(1, 1), target(1, 2), target(1, 0), target(2, 1)] {
        state.transcript_status.insert(target, "Live".into());
    }
    state
}

#[test]
fn activity_is_scoped_to_the_live_agent_and_not_draft_or_session() {
    let mut state = fixture();
    assert!(tab_is_running(&state, target(1, 1)));
    assert!(!tab_is_running(&state, target(1, 2)));
    assert!(!tab_is_running(&state, target(1, 0)));
    assert!(!tab_is_running(&state, target(2, 1)));
    assert!(!tab_is_running(&state, target(1, 3)));

    state.agents.get_mut(&SessionId(1)).unwrap()[0].state = AgentState::Interrupted;
    assert!(!tab_is_running(&state, target(1, 1)));
    state.agents.get_mut(&SessionId(1)).unwrap()[0].state = AgentState::Running;
    state
        .transcript_status
        .insert(target(1, 1), "Reconnecting".into());
    assert!(!tab_is_running(&state, target(1, 1)));
    state.transcript_status.insert(target(1, 1), "Live".into());
    state.connection = "Disconnected".into();
    assert!(!tab_is_running(&state, target(1, 1)));
}

fn tabs_root() -> Element {
    let mut ui = use_signal(|| {
        let mut state = fixture();
        state.workspace.open(target(1, 1), true);
        state.workspace.open_duplicate(target(1, 1), true);
        state.workspace.open(target(1, 2), true);
        state.workspace.open(target(1, 0), true);
        state
    });
    use_context_provider(|| ui);
    let root = ui.read().workspace.root.clone();
    rsx! {
        button { onclick: move |_| ui.write().agents.get_mut(&SessionId(1)).unwrap()[0].state = AgentState::Idle, "Finish agent" }
        Layout { node: root, path: vec![] }
    }
}

#[test]
fn running_tabs_render_accessible_spinners_including_duplicates() {
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(tabs_root);
    let mutations = dom.rebuild_to_vec();
    let finish_button = mutations
        .edits
        .iter()
        .find_map(|edit| match edit {
            dioxus::core::Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .unwrap();
    let html = dioxus_ssr::render(&dom);
    assert_eq!(html.matches("class=\"tab-spinner\"").count(), 2, "{html}");
    assert_eq!(
        html.matches("aria-label=\"Agent running\"").count(),
        2,
        "{html}"
    );
    assert!(html.contains("agent-2"), "{html}");
    assert!(html.contains("Root"), "{html}");
    assert!(crate::style::CSS.contains("prefers-reduced-motion: reduce"));

    use dioxus::html::{PlatformEventData, SerializedMouseData};
    use std::{any::Any, rc::Rc};
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default())) as Rc<dyn Any>,
        true,
    );
    dom.runtime().handle_event("click", event, finish_button);
    dom.render_immediate_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(!html.contains("class=\"tab-spinner\""), "{html}");
}

fn details_root() -> Element {
    let ui = use_signal(|| {
        let mut state = fixture();
        state.sessions.push(serde_json::from_value(serde_json::json!({
            "id":1,"parent_id":null,"name":"session1","working_directory":"/work",
            "probe_id":"00000000-0000-0000-0000-000000000001",
            "model_config":{"model":{"provider":"Openrouter","api_name":"test/model"},"effort":"Medium"},
            "busy":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap());
        state.workspace.open(target(1, 1), false);
        state
    });
    use_context_provider(|| ui);
    rsx! { Details {} }
}

#[test]
fn details_show_running_spinner_without_activity_or_status_text() {
    let mut dom = VirtualDom::new(details_root);
    dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("Current session"), "{html}");
    assert!(html.contains("Show internal"), "{html}");
    assert!(html.contains("details-agents-heading"), "{html}");
    assert!(html.contains("aria-label=\"Agent running\""), "{html}");
    assert!(!html.contains("Activity"), "{html}");
    assert!(!html.contains(" · Running"), "{html}");
    assert!(!html.contains(" · Idle"), "{html}");
}
