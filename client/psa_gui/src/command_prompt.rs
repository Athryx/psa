//! Inline value prompts that slash commands open above the composer line.
use crate::input::{BrowserInput, DraftStore, InputKeyEvent, InputSnapshot};
use dioxus::prelude::*;

/// Edits a single value for a slash command. Enter or Save submits the trimmed
/// value; Escape or Cancel closes without changes. Empty values are not submitted.
#[component]
pub(crate) fn CommandPrompt(
    label: String,
    #[props(default)] placeholder: String,
    #[props(default)] initial_value: String,
    #[props(default = "Save".to_owned())] submit_label: String,
    onsubmit: EventHandler<String>,
    oncancel: EventHandler<()>,
) -> Element {
    let initial = use_signal(|| initial_value.clone());
    let mut value = use_signal(DraftStore::<()>::default);
    let current = move || {
        value
            .peek()
            .get(&())
            .cloned()
            .unwrap_or_else(|| initial.peek().clone())
    };
    let submit = move || {
        let text = current().trim().to_owned();
        if !text.is_empty() {
            onsubmit.call(text);
        }
    };

    rsx! {
        div { class: "command-prompt", role: "group", "aria-label": "{label}",
            span { class: "command-prompt-label", "{label}" }
            div { class: "command-prompt-row",
                BrowserInput {
                    initial_value: initial_value.clone(),
                    mutation: value.read().mutation(&()),
                    aria_label: label.clone(),
                    placeholder,
                    autofocus: true,
                    capture_enter: true,
                    capture_escape: true,
                    onedit: move |snapshot: InputSnapshot| { value.write().apply_snapshot((), snapshot); },
                    onkey: move |event: InputKeyEvent| {
                        if event.snapshot.composing || !value.write().apply_snapshot((), event.snapshot.clone()) {
                            return;
                        }
                        match event.key() {
                            Key::Enter => { event.prevent_default(); submit(); }
                            Key::Escape => { event.prevent_default(); event.stop_propagation(); oncancel.call(()); }
                            _ => {}
                        }
                    },
                }
                button { class: "primary", onmousedown: move |event| event.prevent_default(), onclick: move |_| submit(), "{submit_label}" }
                button { onmousedown: move |event| event.prevent_default(), onclick: move |_| oncancel.call(()), "Cancel" }
            }
            div { class: "command-hint", "Enter to {submit_label.to_lowercase()} · Esc to cancel" }
        }
    }
}
