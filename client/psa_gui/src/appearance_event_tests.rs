use super::*;
use psa_api::events::{EventType, StatusChange};
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
