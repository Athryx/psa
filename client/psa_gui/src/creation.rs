//! Session creation and remote directory selection.
use crate::{
    app::Ui,
    input::BrowserInput,
    network,
    reactive::{use_ui_selector, use_ui_subscription},
};
use dioxus::prelude::*;
use psa_api::*;

#[derive(Clone, PartialEq)]
pub(crate) struct CreationTarget {
    pub probe: ProbeId,
    pub directory: String,
}

pub(crate) fn select_target(ui: &mut Ui, target: CreationTarget) {
    if ui.creation_pending {
        return;
    }
    ui.creation_revision += 1;
    if ui.creation_target.as_ref() != Some(&target) {
        ui.creation_name.clear();
        ui.creation_error = None;
        ui.creation_uncertain = false;
    }
    ui.creation_target = Some(target);
}

#[component]
pub fn InlineSessionName(target: CreationTarget) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| {
        (
            state.generation,
            state.creation_name.clone(),
            state.creation_pending,
            state.creation_uncertain,
            state.creation_error.clone(),
        )
    });
    let generation = ui.peek().generation;
    let state = ui.peek();
    rsx! {
        div { class: "session-create-inline",
            BrowserInput {
                aria_label: "Session name", placeholder: "Session name", initial_value: state.creation_name.clone(), disabled: state.creation_pending, capture_enter:true, capture_escape:true,
                onmounted: move |event: MountedEvent| { let node = event.data(); spawn(async move { let _ = node.set_focus(true).await; }); },
                onedit: move |snapshot: crate::input::InputSnapshot| { if ui.peek().creation_name != snapshot.text { ui.write().creation_name = snapshot.text; } },
                onkey: move |event: crate::input::InputKeyEvent| {
                    if ui.peek().creation_name != event.snapshot.text { ui.write().creation_name = event.snapshot.text.clone(); }
                    if event.key() == Key::Escape {
                        event.prevent_default(); event.stop_propagation();
                        if !ui.read().creation_pending { ui.write().creation_target = None; }
                    } else if event.key() == Key::Enter {
                        event.prevent_default(); event.stop_propagation();
                        let state = ui.read();
                        let trimmed = state.creation_name.trim().to_owned();
                        if state.creation_pending || state.creation_uncertain || trimmed.is_empty() { return; }
                        drop(state);
                        let request = CreateSession { name: trimmed, working_directory: target.directory.as_str().into(), probe_id: target.probe, model_config: None };
                        let api = match network::client(ui) { Ok(api) => api, Err(_) => { ui.write().creation_error = Some("Invalid API connection settings".into()); return; } };
                        ui.write().creation_pending = true;
                        ui.write().creation_error = None;
                        let focus_intent = crate::app::begin_composer_open();
                        dioxus::core::spawn_forever(async move {
                            let result = api.create_session(&request).await;
                            if ui.read().generation != generation { return; }
                            match result {
                                Ok(session) => { let id = session.id; ui.write().upsert_session(session); ui.write().creation_target = None; ui.write().creation_pending = false; spawn(network::open_session_with_focus(ui, id, true, focus_intent)); }
                                Err(_) => { let mut state = ui.write(); state.creation_error = Some("Session creation failed or delivery is uncertain. Check the session list before trying again; nothing was retried automatically. Press Escape to dismiss.".into()); state.creation_uncertain = true; state.creation_pending = false; }
                            }
                        });
                    }
                }
            }
            if let Some(message) = &state.creation_error { p { class: "error", "{message}" } }
        }
    }
}

/// Absolute home directory of an online probe, for a new session's default path.
fn probe_start_directory(probe: &Probe) -> String {
    probe
        .metadata
        .home_dir
        .clone()
        .filter(|path| path.starts_with('/'))
        .unwrap_or_else(|| "/".into())
}

/// The selected probe if it is online, otherwise the first online probe.
fn initial_probe(state: &Ui) -> Option<(ProbeId, String)> {
    state
        .selected_probe
        .and_then(|id| {
            state
                .probes
                .iter()
                .find(|p| p.metadata.id == id && p.is_active)
        })
        .or_else(|| state.probes.iter().find(|p| p.is_active))
        .map(|p| (p.metadata.id, probe_start_directory(p)))
}

#[component]
pub fn CreateSessionPicker() -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut open = use_signal(|| false);
    use_ui_subscription(|state| (state.connection.clone(), state.creation_pending));
    let state = ui.peek();
    rsx! {
        button { class: "primary session-new", title: "New session", disabled: state.connection != "Connected" || state.creation_pending,
            onclick: move |_| { open.set(true); spawn(network::refresh_probes(ui)); }, "New"
        }
        if open() { NewSessionModal { open } }
    }
}

#[component]
fn NewSessionModal(open: Signal<bool>) -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let initial = initial_probe(&ui.peek());
    let mut probe = use_signal(|| initial.as_ref().map(|(id, _)| *id));
    let mut path = use_signal(|| initial.map(|(_, path)| path).unwrap_or_default());
    // Remounts the path input when the probe changes, resetting it to that probe's home.
    let mut path_reset = use_signal(|| 0u64);
    let mut checking = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let available = use_ui_selector(|state| state.probes.clone());
    use_ui_subscription(|state| (state.probes.clone(), state.probe_error.clone()));

    // A probe may come online after the dialog opened (for example, after a
    // server restart). Select it as soon as the background refresh sees it.
    use_effect(move || {
        let _ = available.read();
        if probe.peek().is_some() {
            return;
        }
        if let Some((id, home)) = initial_probe(&ui.peek()) {
            probe.set(Some(id));
            path.set(home);
            path_reset += 1;
        }
    });

    let mut submit = move |requested: String| {
        let requested = requested.trim().to_owned();
        if checking() {
            return;
        }
        if !requested.starts_with('/') {
            error.set(Some("Enter an absolute path on the probe".into()));
            return;
        }
        let Some(id) = *probe.peek() else { return };
        if !ui
            .peek()
            .probes
            .iter()
            .any(|p| p.metadata.id == id && p.is_active)
        {
            return;
        }

        checking.set(true);
        error.set(None);
        let generation = ui.peek().generation;
        spawn(async move {
            let result = match network::client(ui) {
                Ok(api) => api
                    .list_probe_dir(id, &requested)
                    .await
                    .map_err(|e| e.to_string()),
                Err(_) => Err("Invalid API connection settings".into()),
            };
            if ui.peek().generation != generation {
                return;
            }
            checking.set(false);
            if let Err(message) = result {
                error.set(Some(format!("Unable to open directory: {message}")));
                return;
            }

            if ui
                .peek()
                .selected_probe
                .is_some_and(|selected| selected != id)
            {
                let mut state = ui.write();
                state.selected_probe = None;
                state.next_sessions = None;
                drop(state);
                spawn(network::load_sessions(ui, false));
            }
            select_target(
                &mut ui.write(),
                CreationTarget {
                    probe: id,
                    directory: requested,
                },
            );
            open.set(false);
        });
    };

    let state = ui.peek();
    let probes: Vec<_> = state
        .probes
        .iter()
        .map(|p| {
            (
                p.metadata.id,
                crate::views::probe_label(p),
                probe_start_directory(p),
                p.is_active,
            )
        })
        .collect();
    let probe_choices = probes.clone();
    let selected_online = probe().is_some_and(|id| probes.iter().any(|p| p.0 == id && p.3));
    let probe_error = state.probe_error.clone();
    drop(state);

    rsx! {
        crate::modal::Modal { title: "New session", close_label: "Close new session", class: "new-session", onclose: move |_| open.set(false),
            label { r#for: "picker-probe", "Probe" }
            select { id: "picker-probe", value: probe().map(|id| id.to_string()).unwrap_or_default(),
                onchange: move |event| {
                    if let Some((id, _, home, _)) = probe_choices.iter().find(|p| p.0.to_string() == event.value() && p.3) {
                        probe.set(Some(*id));
                        path.set(home.clone());
                        path_reset += 1;
                        error.set(None);
                    }
                },
                for (id, label, _, active) in probes { option { value: id.to_string(), disabled: !active, "{label}" } }
            }
            if let Some(id) = probe() { crate::views::ProbeName { id } }
            label { r#for: "picker-path", "Working directory" }
            {rsx! {
    crate::path_picker::ProbePathInput { key: "{path_reset}", id: "picker-path", probe: probe(), initial: path(), aria_label: "Absolute path on the probe", placeholder: "/home/user/project", disabled: checking(),
                onchange: move |text: String| { path.set(text); error.set(None); },
                onenter: move |text: String| submit(text),
            }
            }}
            if probe().is_none() { p { class: "error", "No online probes available" } }
            if let Some(message) = probe_error { p { class: "error", "{message}" } }
            if probe().is_some() && !selected_online { p { class: "error", "Selected probe is offline. Choose an online probe or wait for it to reconnect." } }
            if let Some(message) = error() { p { class: "error", "{message}" } }
            div { class: "modal-actions",
                button { r#type: "button", onclick: move |_| open.set(false), "Cancel" }
                button { class: "primary", r#type: "button", disabled: !selected_online || checking(), onclick: move |_| submit(path()),
                    if checking() { "Checking…" } else { "Continue" }
                }
            }
        }
    }
}
