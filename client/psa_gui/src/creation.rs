//! Session creation and remote directory selection.
use crate::{app::Ui, network};
use dioxus::prelude::*;
use psa_api::{probes::DirectoryEntry, *};

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
    let generation = ui.read().generation;
    let state = ui.read();
    rsx! {
        div { class: "session-create-inline",
            input {
                "aria-label": "Session name", placeholder: "Session name", value: state.creation_name.clone(), disabled: state.creation_pending,
                onmounted: move |event| { let node = event.data(); spawn(async move { let _ = node.set_focus(true).await; }); },
                oninput: move |event| ui.write().creation_name = event.value(),
                onkeydown: move |event| {
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
                        dioxus::core::spawn_forever(async move {
                            let result = api.create_session(&request).await;
                            if ui.read().generation != generation { return; }
                            match result {
                                Ok(session) => { let id = session.id; ui.write().upsert_session(session); ui.write().creation_target = None; ui.write().creation_pending = false; spawn(network::open_session(ui, id, true)); }
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

fn parent_directory(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
        .unwrap_or("/")
        .to_owned()
}

fn load_directory(
    ui: Signal<Ui>,
    probe: ProbeId,
    path: String,
    mut entries: Signal<Vec<DirectoryEntry>>,
    mut loading: Signal<bool>,
    mut error: Signal<Option<String>>,
    mut revision: Signal<u64>,
) {
    let next = revision() + 1;
    revision.set(next);
    entries.set(Vec::new());
    error.set(None);
    loading.set(true);
    let generation = ui.read().generation;
    spawn(async move {
        let result = match network::client(ui) {
            Ok(api) => api
                .list_probe_dir(probe, &path)
                .await
                .map_err(|e| e.to_string()),
            Err(_) => Err("Invalid API connection settings".into()),
        };
        if ui.read().generation != generation || revision() != next {
            return;
        }
        loading.set(false);
        match result {
            Ok(mut found) => {
                // A symlink may point to a directory; browsing it lets the probe decide.
                found.retain(|e| e.kind == "directory" || e.kind == "symlink");
                found.sort_by(|a, b| a.name.cmp(&b.name));
                entries.set(found);
            }
            Err(message) => error.set(Some(format!("Unable to browse directory: {message}"))),
        }
    });
}

#[component]
pub fn CreateSessionPicker() -> Element {
    let mut ui = use_context::<Signal<Ui>>();
    let mut open = use_signal(|| false);
    crate::app::register_modal(open);
    let mut probe = use_signal(|| None::<ProbeId>);
    let mut path = use_signal(|| "/".to_string());
    let mut path_input = use_signal(|| "/".to_string());
    let mut entries = use_signal(Vec::<DirectoryEntry>::new);
    let mut loading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut revision = use_signal(|| 0u64);
    // A probe may come online after the picker opened (for example, after a
    // server restart). Select it as soon as the background refresh sees it.
    use_effect(move || {
        if !open() || probe().is_some() {
            return;
        }
        let state = ui.read();
        let initial = state
            .selected_probe
            .and_then(|id| {
                state
                    .probes
                    .iter()
                    .find(|p| p.metadata.id == id && p.is_active)
            })
            .or_else(|| state.probes.iter().find(|p| p.is_active))
            .map(|p| {
                (
                    p.metadata.id,
                    p.metadata
                        .home_dir
                        .clone()
                        .filter(|path| path.starts_with('/'))
                        .unwrap_or_else(|| "/".into()),
                )
            });
        drop(state);
        if let Some((id, home)) = initial {
            probe.set(Some(id));
            path.set(home.clone());
            path_input.set(home.clone());
            load_directory(ui, id, home, entries, loading, error, revision);
        }
    });
    let state = ui.read();
    let probes: Vec<_> = state
        .probes
        .iter()
        .map(|p| {
            (
                p.metadata.id,
                crate::views::probe_label(p),
                p.metadata
                    .home_dir
                    .clone()
                    .filter(|p| p.starts_with('/'))
                    .unwrap_or_else(|| "/".into()),
                p.is_active,
            )
        })
        .collect();
    let probe_choices = probes.clone();
    let selected_online = probe().is_some_and(|id| probes.iter().any(|p| p.0 == id && p.3));
    let initial = state
        .selected_probe
        .and_then(|id| probes.iter().find(|p| p.0 == id && p.3))
        .or_else(|| probes.iter().find(|p| p.3))
        .cloned();
    rsx! {
        button { class: "primary session-new", title: "New session", disabled: state.connection != "Connected" || state.creation_pending,
            onclick: move |_| {
                open.set(true);
                spawn(network::refresh_probes(ui));
                if let Some((id, _, home, _)) = initial.clone() { probe.set(Some(id)); path.set(home.clone()); path_input.set(home.clone()); load_directory(ui, id, home, entries, loading, error, revision); }
                else { probe.set(None); error.set(Some("No online probes available".into())); }
            }, "New"
        }
        if open() {
            div { class: "modal-backdrop", onclick: move |event| { event.stop_propagation(); revision += 1; open.set(false); },
                section { class: "settings directory-picker", role: "dialog", "aria-modal": "true", "aria-label": "Choose working directory", tabindex: "-1",
                    onclick: move |event| event.stop_propagation(),
                    onmounted: move |event| { let node = event.data(); spawn(async move { let _ = node.set_focus(true).await; }); },
                    onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); revision += 1; open.set(false); } },
                    header { h2 { "Choose working directory" } button { onclick: move |_| { revision += 1; open.set(false); }, "Close ×" } }
                    label { r#for: "picker-probe", "Probe" }
                    select { id: "picker-probe", value: probe().map(|id| id.to_string()).unwrap_or_default(),
                        onchange: move |event| {
                            if let Some((id, _, home, _)) = probe_choices.iter().find(|p| p.0.to_string() == event.value() && p.3) {
                                probe.set(Some(*id)); path.set(home.clone()); path_input.set(home.clone()); load_directory(ui, *id, home.clone(), entries, loading, error, revision);
                            }
                        },
                        for (id, label, _, active) in probes.clone() { option { value: id.to_string(), disabled: !active, "{label}" } }
                    }
                    if let Some(id) = probe() { crate::views::ProbeName { id }
                    }
                    label { r#for: "picker-path", "Absolute path on the probe (press Enter to browse)" }
                    input { id: "picker-path", value: path_input(), oninput: move |event| path_input.set(event.value()),
                        onkeydown: move |event| { if event.key() == Key::Enter { event.prevent_default(); event.stop_propagation(); let requested = path_input(); if !requested.starts_with('/') { revision += 1; loading.set(false); entries.set(Vec::new()); error.set(Some("Enter an absolute path on the probe".into())); return; } if let Some(id) = probe() { path.set(requested.clone()); load_directory(ui, id, requested, entries, loading, error, revision); } } }
                    }
                    button { disabled: loading() || path() == "/", onclick: move |_| { if let Some(id) = probe() { let parent = parent_directory(&path()); path.set(parent.clone()); path_input.set(parent.clone()); load_directory(ui, id, parent, entries, loading, error, revision); } }, "↑ Parent" }
                    if loading() { p { class: "muted", "Loading directories…" } }
                    if let Some(message) = &state.probe_error { p { class: "error", "{message}" } }
                    if let Some(message) = error() { p { class: "error", "{message}" } button { onclick: move |_| { if let Some(id) = probe() { load_directory(ui, id, path(), entries, loading, error, revision); } else { spawn(network::refresh_probes(ui)); } }, "Retry" } }
                    if probe().is_some() && !selected_online { p { class: "error", "Selected probe is offline. Choose an online probe or wait for it to reconnect." } }
                    if !loading() && error().is_none() && probe().is_some() && entries().is_empty() { p { class: "muted", "No directories in this folder." } }
                    div { class: "directory-list",
                        for entry in entries() {
                            button { key: "{entry.name}", onclick: { let entry = entry.clone(); move |_| { if let Some(id) = probe() { let child = if path() == "/" { format!("/{}", entry.name) } else { format!("{}/{}", path(), entry.name) }; path.set(child.clone()); path_input.set(child.clone()); load_directory(ui, id, child, entries, loading, error, revision); } } }, if entry.kind == "symlink" { "↗ " } else { "📁 " } "{entry.name}" if entry.kind == "symlink" { " (symlink)" } }
                        }
                    }
                    button { class: "primary", disabled: !selected_online || loading() || error().is_some() || path_input() != path(), onclick: move |_| { if let Some(id) = probe() { let needs_filter_reset = ui.read().selected_probe.is_some_and(|selected| selected != id); if needs_filter_reset { ui.write().selected_probe = None; ui.write().next_sessions = None; spawn(network::load_sessions(ui, false)); } select_target(&mut ui.write(), CreationTarget { probe: id, directory: path() }); revision += 1; open.set(false); } }, "Select this directory" }
                }
            }
        }
    }
}
