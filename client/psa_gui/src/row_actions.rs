//! Right-click menus and in-place renaming for sidebar rows.
use crate::input::{BrowserInput, InputKeyEvent};
use dioxus::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_RENAME: AtomicU64 = AtomicU64::new(0);

/// One action in a [`RowMenu`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowMenuItem {
    pub label: &'static str,
    pub disabled: bool,
}

impl RowMenuItem {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            disabled: false,
        }
    }
}

/// A context menu at a pointer position. Choosing an item reports its index;
/// Escape, a click elsewhere, or another right-click closes it.
#[component]
pub(crate) fn RowMenu(
    x: f64,
    y: f64,
    items: Vec<RowMenuItem>,
    onselect: EventHandler<usize>,
    onclose: EventHandler<()>,
) -> Element {
    let height = items.len() * 32 + 8;
    rsx! {
        div { class: "row-menu-backdrop",
            onclick: move |event| { event.stop_propagation(); onclose.call(()); },
            oncontextmenu: move |event| { event.prevent_default(); event.stop_propagation(); onclose.call(()); },
        }
        div { class: "row-menu", role: "menu", tabindex: "-1",
            style: "left:min({x}px,calc(100vw - 168px));top:min({y}px,calc(100vh - {height}px))",
            onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { spawn(async move { let _ = event.data().set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.prevent_default(); event.stop_propagation(); onclose.call(()); } },
            for (index, item) in items.into_iter().enumerate() {
                button { r#type: "button", role: "menuitem", disabled: item.disabled,
                    onclick: move |_| onselect.call(index),
                    "{item.label}"
                }
            }
        }
    }
}

/// Replaces a row's label with an input holding the current name, selected so
/// typing replaces it. Enter submits a changed nonempty name; Escape or moving
/// focus away cancels.
#[component]
pub(crate) fn InlineRename(
    initial: String,
    aria_label: String,
    #[props(default)] class: String,
    #[props(default)] style: String,
    onsubmit: EventHandler<String>,
    oncancel: EventHandler<()>,
) -> Element {
    let id = use_hook(|| format!("psa-rename-{}", NEXT_RENAME.fetch_add(1, Ordering::Relaxed)));
    // Enter and Escape both unmount the input, which also blurs it.
    let mut finished = use_signal(|| false);
    let original = initial.clone();
    let select_id = id.clone();

    rsx! {
        div { class: "inline-rename {class}", style: "{style}",
            onfocusout: move |_| { if !finished() { finished.set(true); oncancel.call(()); } },
            onclick: move |event| event.stop_propagation(),
            oncontextmenu: move |event| event.stop_propagation(),
            BrowserInput {
                id: id.clone(), aria_label, initial_value: initial,
                capture_enter: true, capture_escape: true,
                onmounted: move |_| {
                    document::eval(&format!(
                        "requestAnimationFrame(() => {{ const el = document.getElementById({select_id:?}); if (el) {{ el.focus(); el.select(); }} }});"
                    ));
                },
                onkey: move |event: InputKeyEvent| {
                    if event.snapshot.composing || finished() {
                        return;
                    }
                    match event.key() {
                        Key::Escape => {
                            event.prevent_default();
                            event.stop_propagation();
                            finished.set(true);
                            oncancel.call(());
                        }
                        Key::Enter => {
                            event.prevent_default();
                            event.stop_propagation();
                            let name = event.snapshot.text.trim().to_owned();
                            finished.set(true);
                            if name.is_empty() || name == original {
                                oncancel.call(());
                            } else {
                                onsubmit.call(name);
                            }
                        }
                        _ => {}
                    }
                },
            }
        }
    }
}
