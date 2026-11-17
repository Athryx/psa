//! Single-choice probe picker with search, styled like the session filter menu.
//!
//! It expands inline rather than floating, so it is never clipped by a scrolling
//! modal. Offline probes are listed but cannot be chosen.
use crate::{
    app::Ui,
    reactive::use_ui_subscription,
    session_filter::{ProbeOption, build_probe_groups, build_probe_option, check_mark},
};
use dioxus::prelude::*;
use psa::api::ProbeId;

fn is_probe_online(state: &Ui, id: ProbeId) -> bool {
    state
        .probes
        .iter()
        .any(|p| p.metadata.id == id && p.is_active)
}

#[component]
pub(crate) fn ProbePicker(
    id: String,
    selected: Option<ProbeId>,
    onchange: EventHandler<ProbeId>,
) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut open = use_signal(|| false);
    let mut search = use_signal(String::new);
    use_ui_subscription(|state| (state.probes.clone(), state.sandboxes.clone()));

    let state = ui.peek();
    let current = selected.map(|id| build_probe_option(&state, id, "Unknown probe"));
    let query = search();
    let groups: Vec<_> = build_probe_groups(&state)
        .iter()
        .filter_map(|group| group.matching(&query))
        .collect();
    let options: Vec<(ProbeOption, bool, bool)> = groups
        .iter()
        .flat_map(|group| {
            let manager = group.manager.id;
            group
                .options()
                .map(move |option| (option.clone(), option.id != manager))
        })
        .map(|(option, nested)| {
            let online = is_probe_online(&state, option.id);
            (option, nested, online)
        })
        .collect();
    drop(state);

    rsx! {
        div { class: "probe-picker",
            button { id: "{id}", r#type: "button", class: "probe-picker-button", "aria-haspopup": "listbox", "aria-expanded": open(),
                title: current.as_ref().map(|c| c.label.clone()).unwrap_or_default(),
                onclick: move |_| { search.set(String::new()); open.toggle(); },
                match &current {
                    Some(option) => rsx! {
                        span { class: "probe-dot {option.status}", "aria-hidden": "true" }
                        span { class: "filter-option-label", "{option.label}" }
                    },
                    None => rsx! { span { class: "filter-option-label muted", "Choose a probe" } },
                }
                span { class: "probe-picker-caret", "aria-hidden": "true", if open() { "▴" } else { "▾" } }
            }
            if open() {
                div { class: "probe-picker-menu",
                    onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); open.set(false); } },
                    input { r#type: "search", class: "filter-menu-search", placeholder: "Find probes", "aria-label": "Find probes",
                        value: "{query}", oninput: move |event| search.set(event.value()),
                        onmounted: move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
                    }
                    div { role: "listbox", "aria-label": "Probes",
                        if options.is_empty() { p { class: "muted filter-empty", "No matching probes" } }
                        for (option, nested, online) in options {
                            {
                                let checked = selected == Some(option.id);
                                rsx! {
                                    button { key: "{option.id}", r#type: "button", role: "option", "aria-selected": checked, disabled: !online,
                                        class: if nested { "filter-option nested" } else { "filter-option" },
                                        title: if online { option.label.clone() } else { format!("{} (offline)", option.label) },
                                        onclick: move |_| { open.set(false); onchange.call(option.id); },
                                        span { class: "filter-check", "aria-hidden": "true", "{check_mark(checked, false)}" }
                                        span { class: "probe-dot {option.status}", "aria-hidden": "true" }
                                        span { class: "filter-option-label", "{option.label}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
