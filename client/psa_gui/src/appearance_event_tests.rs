use super::*;
use psa::api::events::{EventType, StatusChange};
use std::{collections::HashMap, rc::Rc};

#[derive(Props, Clone)]
struct AppearanceRootProps {
    kind: EventType,
    hide_results: bool,
    hide_statuses: bool,
}
impl PartialEq for AppearanceRootProps {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}
#[allow(non_snake_case)]
fn AppearanceRoot(props: AppearanceRootProps) -> Element {
    let AppearanceRootProps {
        kind,
        hide_results,
        hide_statuses,
    } = props;
    let ui = use_signal(|| {
        let mut settings = DesktopSettings::default();
        settings.preferences.hide_text_recorded_results = hide_results;
        settings.preferences.hide_agent_statuses = hide_statuses;
        Ui::new(settings)
    });
    use_context_provider(|| ui);
    rsx! { EventRow { kind, provisional: false, results: Rc::new(HashMap::new()) } }
}

#[test]
fn recorded_results_respect_text_preference_but_json_remains_expandable() {
    for hide_results in [true, false] {
        let kind = EventType::Result(AgentResult::Text("text artifact".into()));
        let html = dioxus_ssr::render_element(
            rsx! { AppearanceRoot { kind, hide_results, hide_statuses: true } },
        );
        assert_eq!(html.contains("text artifact"), !hide_results, "{html}");
        let kind = EventType::Result(AgentResult::Json(
            serde_json::json!({"key":"structured artifact"}),
        ));
        let html = dioxus_ssr::render_element(
            rsx! { AppearanceRoot { kind, hide_results, hide_statuses: true } },
        );
        assert!(
            html.contains("<details") && html.contains("structured artifact"),
            "{html}"
        );
    }
}

#[test]
fn interrupted_status_is_always_visible_as_error() {
    for hide_statuses in [true, false] {
        let kind = EventType::StatusChange(StatusChange {
            new_status: AgentState::Interrupted,
            reason: None,
        });
        let html = dioxus_ssr::render_element(
            rsx! { AppearanceRoot { kind, hide_results: true, hide_statuses } },
        );
        assert!(
            html.contains("Agent Interrupted") && html.contains("class=\"error\""),
            "{html}"
        );
        let kind = EventType::StatusChange(StatusChange {
            new_status: AgentState::Running,
            reason: None,
        });
        let html = dioxus_ssr::render_element(
            rsx! { AppearanceRoot { kind, hide_results: true, hide_statuses } },
        );
        assert_eq!(html.contains("Agent Running"), !hide_statuses, "{html}");
    }
}

#[test]
fn commands_render_as_right_aligned_command_rows() {
    let kind = EventType::Command(psa::api::events::CommandRecord {
        name: "compact".into(),
        args: "tests".into(),
        outcome: psa::api::CommandOutcome::Succeeded,
        output: None,
    });
    let html = dioxus_ssr::render_element(
        rsx! { AppearanceRoot { kind, hide_results: true, hide_statuses: true } },
    );
    assert!(html.contains("class=\"message command-message\""), "{html}");
    assert!(html.contains("/compact tests"), "{html}");
    assert!(!html.contains("class=\"role\""), "{html}");
}

#[test]
fn received_messages_are_collapsed_rows_naming_the_sender() {
    let kind = EventType::Message(psa::api::events::Message {
        source: Some(psa::api::events::AgentMessageLink {
            request_id: "r1".into(),
            sender: AgentId(7),
            sender_name: "researcher".into(),
            recipient: AgentId(1),
            recipient_name: "recipient".into(),
        }),
        ..psa::api::events::Message::text(psa::api::events::MessageType::Subagent, "found it\nmore")
    });
    let html = dioxus_ssr::render_element(
        rsx! { AppearanceRoot { kind, hide_results: true, hide_statuses: true } },
    );
    assert!(html.contains("Message from"), "{html}");
    assert!(html.contains("researcher"), "{html}");
    assert!(html.contains("<details class=\"item received\""), "{html}");
    assert!(!html.contains("agent-message"), "{html}");
}

fn linked_row_root() -> Element {
    let ui = use_signal(|| Ui::new(DesktopSettings::default()));
    use_context_provider(|| ui);
    let payload = use_hook(|| {
        let message = psa::api::events::Message {
            source: Some(psa::api::events::AgentMessageLink {
                request_id: "r1".into(),
                sender: AgentId(7),
                sender_name: "researcher".into(),
                recipient: AgentId(1),
                recipient_name: "recipient".into(),
            }),
            ..psa::api::events::Message::text(psa::api::events::MessageType::Subagent, "found it")
        };
        let envelope: EventEnvelope = serde_json::from_value(serde_json::json!({
            "session_id": 3, "agent_id": 1, "position": 1,
            "event": {
                "id": 1, "source_agent_id": 1, "timestamp": "2026-01-01T00:00:00Z",
                "kind": serde_json::to_value(EventType::Message(message)).unwrap(),
            },
        }))
        .unwrap();
        RowPayload::Committed(1, std::sync::Arc::new(envelope))
    });
    rsx! {
        CachedEventRow { payload, result: None, preferences: RowPreferences::from(&UiPreferences::default()) }
        output { "{ui.read().pending_reveals.len()}" }
    }
}

#[test]
fn sender_links_open_the_sender_at_the_sending_call() {
    use dioxus::core::Mutation;
    use dioxus::html::{PlatformEventData, SerializedMouseData};
    dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
    let mut dom = VirtualDom::new(linked_row_root);
    let edits = dom.rebuild_to_vec();
    let html = dioxus_ssr::render(&dom);
    assert!(html.contains("class=\"text-link agent-link\""), "{html}");
    // The summary toggle registers first; the link's click listener follows it.
    let clicks: Vec<_> = edits
        .edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == "click" => Some(*id),
            _ => None,
        })
        .collect();
    let event = Event::new(
        Rc::new(PlatformEventData::new(Box::<SerializedMouseData>::default()))
            as Rc<dyn std::any::Any>,
        true,
    );
    dom.runtime()
        .handle_event("click", event, *clicks.last().unwrap());
    dom.render_immediate_to_vec();
    let ui = dom.in_scope(ScopeId::APP, consume_context::<Signal<Ui>>);
    let state = ui.peek();
    let target = TranscriptTarget {
        session_id: SessionId(3),
        agent_id: AgentId(7),
    };
    assert_eq!(
        state.pending_reveals.get(&target),
        Some(&MessageLink::Sent {
            request_id: "r1".into()
        })
    );
    assert_eq!(
        state.workspace.active_tab().map(|tab| tab.target),
        Some(target)
    );
}
