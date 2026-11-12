//! Shared modal chrome and the modal stack.
//!
//! Every [`Modal`] registers with the app's `ModalRegistry`. Modals opened from
//! inside another modal stack above it: only the top one reacts to Escape or a
//! backdrop click, and closing it returns focus to whatever opened it.
use crate::app::{ModalRegistry, register_modal};
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

/// A stacked dialog. Render it only while open; `onclose` should unmount it.
#[component]
pub(crate) fn Modal(
    title: String,
    close_label: String,
    onclose: EventHandler<()>,
    /// Extra classes for the dialog, after `settings`.
    #[props(default)]
    class: String,
    children: Element,
) -> Element {
    let registry = dioxus::core::try_consume_context::<ModalRegistry>();
    let token = register_modal(use_signal(|| true));
    let depth = registry
        .as_ref()
        .map_or(0, |registry| registry.depth(&token));
    let is_top = {
        let token = token.clone();
        move || {
            registry
                .as_ref()
                .is_none_or(|registry| registry.is_top(&token))
        }
    };
    let close_if_top = {
        let is_top = is_top.clone();
        move || {
            if is_top() {
                onclose.call(());
            }
        }
    };
    let backdrop_close = close_if_top.clone();
    let escape_close = close_if_top;

    // Restore focus to the opener, in stack order, when this modal unmounts.
    use_drop(|| {
        document::eval("const el = window.psaModalFocus?.pop(); if (el?.isConnected) el.focus();");
    });

    rsx! {
        div { class: "modal-backdrop", style: "z-index:{10 + depth}",
            onclick: move |event| { event.stop_propagation(); backdrop_close(); },
            section { class: "settings {class}", role: "dialog", "aria-modal": "true", "aria-label": "{title}", tabindex: "-1",
                onclick: move |event| event.stop_propagation(),
                onmounted: move |event| {
                    document::eval("(window.psaModalFocus ??= []).push(document.activeElement);");
                    let node = event.data();
                    spawn(async move { let _ = node.set_focus(true).await; });
                },
                onkeydown: move |event| {
                    if event.key() == Key::Escape {
                        event.prevent_default();
                        event.stop_propagation();
                        escape_close();
                    }
                },
                ModalHeader { title: title.clone(), close_label, onclose: move |_| onclose.call(()) }
                {children}
            }
        }
    }
}
