//! A probe path text input with a Browse button that opens a modal directory browser.
use crate::{
    app::Ui,
    input::{BrowserInput, DraftStore, InputKeyEvent, InputSnapshot},
    modal::Modal,
    network,
};
use dioxus::prelude::*;
use psa_api::{ProbeId, probes::DirectoryEntry};

pub(crate) fn parent_directory(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
        .unwrap_or("/")
        .to_owned()
}

fn child_directory(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

/// Free-text absolute path on `probe`, plus a Browse button. `initial` is read
/// once at mount; remount with a new `key` to reset it from outside.
#[component]
pub(crate) fn ProbePathInput(
    probe: Option<ProbeId>,
    #[props(default)] id: String,
    #[props(default)] initial: String,
    #[props(default)] placeholder: String,
    aria_label: String,
    #[props(default)] disabled: bool,
    /// Every change, typed or chosen.
    onchange: EventHandler<String>,
    /// A path chosen in the browser, after `onchange`.
    #[props(default)]
    onselect: EventHandler<String>,
    /// Enter pressed in the text box.
    #[props(default)]
    onenter: EventHandler<String>,
) -> Element {
    let mut input = use_signal(|| {
        let mut store = DraftStore::<()>::default();
        if !initial.is_empty() {
            store.insert((), initial.clone());
        }
        store
    });
    let mut browsing = use_signal(|| false);
    let current = move || input.peek().get(&()).cloned().unwrap_or_default();
    let initial_value = input.peek().get(&()).cloned().unwrap_or_default();
    let browse_label = format!("Browse for {aria_label}");

    rsx! {
        div { class: "path-input",
            BrowserInput { id, aria_label, placeholder, disabled, capture_enter: true, initial_value, mutation: input.read().mutation(&()),
                onedit: move |snapshot: InputSnapshot| {
                    if input.write().apply_snapshot((), snapshot) { onchange.call(current()); }
                },
                onkey: move |event: InputKeyEvent| {
                    if !input.write().apply_snapshot((), event.snapshot.clone()) { return; }
                    onchange.call(current());
                    if event.key() == Key::Enter {
                        event.prevent_default();
                        event.stop_propagation();
                        onenter.call(event.snapshot.text.clone());
                    }
                }
            }
            button { r#type: "button", class: "path-browse", title: "{browse_label}", "aria-label": "{browse_label}",
                disabled: disabled || probe.is_none(), onclick: move |_| browsing.set(true), "Browse…" }
        }
        if browsing() {
            if let Some(probe) = probe {
                DirectoryBrowserModal { probe, start: current(),
                    onselect: move |path: String| {
                        input.write().insert((), path.clone());
                        browsing.set(false);
                        onchange.call(path.clone());
                        onselect.call(path);
                    },
                    onclose: move |_| browsing.set(false),
                }
            }
        }
    }
}

/// Fixed-size modal for choosing a directory on a probe. Previous entries stay
/// visible (dimmed) while the next directory loads, so the layout never jumps.
#[component]
pub(crate) fn DirectoryBrowserModal(
    probe: ProbeId,
    #[props(default)] start: String,
    onselect: EventHandler<String>,
    onclose: EventHandler<()>,
) -> Element {
    let ui = use_context::<Signal<Ui>>();
    let mut path = use_signal(String::new);
    let mut path_input = use_signal(DraftStore::<()>::default);
    let mut entries = use_signal(Vec::<DirectoryEntry>::new);
    let mut loading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut revision = use_signal(|| 0u64);

    let mut browse = move |next: String| {
        let request = revision() + 1;
        revision.set(request);
        path.set(next.clone());
        path_input.write().insert((), next.clone());
        loading.set(true);
        error.set(None);
        let generation = ui.peek().generation;
        spawn(async move {
            let result = match network::client(ui) {
                Ok(api) => api
                    .list_probe_dir(probe, &next)
                    .await
                    .map_err(|e| e.to_string()),
                Err(_) => Err("Invalid API connection settings".into()),
            };
            if ui.peek().generation != generation || *revision.peek() != request {
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
                Err(message) => {
                    entries.set(Vec::new());
                    error.set(Some(format!("Unable to browse directory: {message}")));
                }
            }
        });
    };

    use_hook(move || {
        let home = ui
            .peek()
            .probes
            .iter()
            .find(|p| p.metadata.id == probe)
            .and_then(|p| p.metadata.home_dir.clone());
        let first = [Some(start.clone()), home]
            .into_iter()
            .flatten()
            .find(|path| path.starts_with('/'))
            .unwrap_or_else(|| "/".into());
        browse(first);
    });

    let probe_name = ui
        .peek()
        .probes
        .iter()
        .find(|p| p.metadata.id == probe)
        .map(crate::views::probe_label)
        .unwrap_or_else(|| "probe".into());
    let can_select = !loading() && error().is_none();

    rsx! {
        Modal { title: "Choose directory on {probe_name}", close_label: "Close directory browser", class: "file-browser", onclose: move |_| onclose.call(()),
            div { class: "file-browser-bar",
                button { r#type: "button", title: "Parent directory", "aria-label": "Parent directory", disabled: path() == "/",
                    onclick: move |_| browse(parent_directory(&path())), "↑" }
                BrowserInput { aria_label: "Directory path", capture_enter: true, initial_value: path_input.peek().get(&()).cloned().unwrap_or_default(), mutation: path_input.read().mutation(&()),
                    onedit: move |snapshot: InputSnapshot| { path_input.write().apply_snapshot((), snapshot); },
                    onkey: move |event: InputKeyEvent| {
                        if !path_input.write().apply_snapshot((), event.snapshot.clone()) { return; }
                        if event.key() != Key::Enter { return; }
                        event.prevent_default();
                        event.stop_propagation();
                        let requested = event.snapshot.text.trim().to_owned();
                        if requested.starts_with('/') {
                            browse(requested);
                        } else {
                            error.set(Some("Enter an absolute path".into()));
                        }
                    }
                }
            }
            div { class: if loading() { "file-browser-list loading" } else { "file-browser-list" }, "aria-busy": loading(),
                if let Some(message) = error() {
                    div { class: "file-browser-message",
                        p { class: "error", "{message}" }
                        button { r#type: "button", onclick: move |_| browse(path()), "Retry" }
                    }
                } else if !loading() && entries.read().is_empty() {
                    p { class: "file-browser-message muted", "No directories in this folder." }
                }
                for entry in entries() {
                    button { key: "{entry.name}", r#type: "button", class: "file-browser-entry", disabled: loading(),
                        onclick: { let name = entry.name.clone(); move |_| browse(child_directory(&path(), &name)) },
                        span { class: "file-browser-icon", if entry.kind == "symlink" { "↗" } else { "📁" } }
                        "{entry.name}"
                    }
                }
                if loading() { div { class: "file-browser-loading", span { class: "tab-spinner", role: "img", "aria-label": "Loading" } } }
            }
            footer { class: "file-browser-footer",
                span { class: "muted file-browser-selected", title: "{path()}", "{path()}" }
                button { r#type: "button", onclick: move |_| onclose.call(()), "Cancel" }
                button { r#type: "button", class: "primary", disabled: !can_select, onclick: move |_| onselect.call(path()), "Select" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_and_child_paths_stay_absolute() {
        assert_eq!(parent_directory("/a/b"), "/a");
        assert_eq!(parent_directory("/a"), "/");
        assert_eq!(parent_directory("/"), "/");
        assert_eq!(child_directory("/", "a"), "/a");
        assert_eq!(child_directory("/a/", "b"), "/a/b");
    }
}
