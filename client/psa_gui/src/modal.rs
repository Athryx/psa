//! Shared modal chrome, so every dialog has the same title row and close control.
use dioxus::prelude::*;

/// Title, icon close button, and divider for a `.settings` dialog.
#[component]
pub(crate) fn ModalHeader(
    title: String,
    close_label: String,
    onclose: EventHandler<()>,
) -> Element {
    rsx! {
        header { class: "modal-header",
            h2 { "{title}" }
            button { class: "modal-close", "aria-label": "{close_label}", title: "Close", onclick: move |_| onclose.call(()), "×" }
        }
    }
}
