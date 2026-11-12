//! Sidebar pane listing probes and the sandboxes each manager probe runs.
use crate::{app::Ui, reactive::use_ui_subscription, views::probe_label};
use dioxus::prelude::*;
use psa_api::{Probe, ProbeId, Sandbox};
use std::collections::HashSet;

#[component]
pub fn ProbesPane() -> Element {
    let ui = use_context::<Signal<Ui>>();
    use_ui_subscription(|state| {
        (
            state.probes.clone(),
            state.sandboxes.clone(),
            state.probe_error.clone(),
            state.generation,
        )
    });
    let state = ui.peek();

    // Sandbox probes are listed under their manager rather than at top level.
    let sandbox_ids: HashSet<ProbeId> = state.sandboxes.iter().map(|s| s.probe_id).collect();
    let probes: Vec<(Probe, Vec<Sandbox>)> = state
        .probes
        .iter()
        .filter(|probe| !sandbox_ids.contains(&probe.metadata.id))
        .map(|probe| {
            let sandboxes = state
                .sandboxes
                .iter()
                .filter(|sandbox| sandbox.manager_id == probe.metadata.id)
                .cloned()
                .collect();
            (probe.clone(), sandboxes)
        })
        .collect();
    let probe_error = state.probe_error.clone();
    drop(state);

    rsx! {
        div { class: "sessions-heading",
            h2 { "Probes" }
        }
        if let Some(message) = probe_error { p { class: "error", "{message}" } }
        if probes.is_empty() { p { class: "muted", "No probes registered." } }
        for (probe, sandboxes) in probes {
            ProbeEntry { key: "{probe.metadata.id}", probe, sandboxes }
        }
    }
}

#[component]
fn ProbeEntry(probe: Probe, sandboxes: Vec<Sandbox>) -> Element {
    let name = probe_label(&probe);
    let id = probe.metadata.id;
    let can_create = probe.metadata.sandboxes && probe.is_active;
    rsx! {
        div { class: "probe-entry",
            div { class: "probe-row",
                crate::views::ProbeName { id }
                span { class: "muted probe-meta", "{probe.metadata.os}/{probe.metadata.architecture}" }
                if probe.metadata.host { span { class: "probe-badge", "host" } }
            }
            if probe.metadata.sandboxes {
                div { class: "sandbox-list",
                    for sandbox in sandboxes {
                        SandboxEntry { key: "{sandbox.probe_id}", sandbox }
                    }
                    if can_create {
                        crate::sandbox_modal::CreateSandboxButton { manager: id, manager_name: name.clone() }
                    } else if !probe.is_active {
                        p { class: "muted sandbox-hint", "Manager offline; sandboxes cannot be changed." }
                    }
                }
            }
        }
    }
}

#[component]
fn SandboxEntry(sandbox: Sandbox) -> Element {
    rsx! {
        div { class: "probe-row sandbox-entry",
            crate::views::ProbeName { id: sandbox.probe_id }
            span { class: "muted probe-meta sandbox-image", title: "{sandbox.image}", "{sandbox.image}" }
        }
    }
}
