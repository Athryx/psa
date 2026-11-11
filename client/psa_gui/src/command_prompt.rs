//! Inline value prompts that slash commands open above the composer line.
use crate::input::{BrowserInput, InputKeyEvent, InputSnapshot};
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
    // The browser owns the live text; this mirrors its latest packet for Save.
    let mut value = use_signal(|| initial_value.clone());
    let submit = move |text: &str| {
        let text = text.trim();
        if !text.is_empty() {
            onsubmit.call(text.to_owned());
        }
    };

    rsx! {
        div { class: "command-prompt", role: "group", "aria-label": "{label}",
            span { class: "command-prompt-label", "{label}" }
            div { class: "command-prompt-row",
                BrowserInput {
                    initial_value: initial_value.clone(),
                    aria_label: label.clone(),
                    placeholder,
                    autofocus: true,
                    capture_enter: true,
                    capture_escape: true,
                    onedit: move |snapshot: InputSnapshot| value.set(snapshot.text),
                    onkey: move |event: InputKeyEvent| {
                        if event.snapshot.composing {
                            return;
                        }
                        value.set(event.snapshot.text.clone());
                        match event.key() {
                            // Cancelling needs no text, so it never waits on a packet.
                            Key::Escape => { event.prevent_default(); event.stop_propagation(); oncancel.call(()); }
                            // Submit the text as it was when the key was pressed.
                            Key::Enter => { event.prevent_default(); submit(&event.snapshot.text); }
                            _ => {}
                        }
                    },
                }
                button { class: "primary", onmousedown: move |event| event.prevent_default(), onclick: move |_| submit(&value.peek()), "{submit_label}" }
                button { onmousedown: move |event| event.prevent_default(), onclick: move |_| oncancel.call(()), "Cancel" }
            }
            div { class: "command-hint", "Enter to {submit_label.to_lowercase()} · Esc to cancel" }
        }
    }
}
