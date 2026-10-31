//! Browser-owned high-frequency interactions. Rust only receives semantic commits.
use client_shared::SplitAxis;
use dioxus::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

const SCRIPT: &str = include_str!("interaction.js");
static NEXT_CONTROL: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize)]
struct Commit {
    value: f64,
    revision: u64,
}

fn use_control() -> (String, Rc<Cell<u64>>) {
    let token = use_hook(|| {
        format!(
            "psa-interaction-{}",
            NEXT_CONTROL.fetch_add(1, Ordering::Relaxed)
        )
    });
    let revision = use_hook(|| Rc::new(Cell::new(0)));
    let cleanup_token = token.clone();
    use_drop(move || {
        document::eval(&format!(
            "window.psaInteractions?.remove({});",
            json!(cleanup_token)
        ));
    });
    (token, revision)
}

fn mount_control(
    kind: &str,
    token: &str,
    options: &str,
    revision: Rc<Cell<u64>>,
    oncommit: EventHandler<f64>,
) {
    let script = format!(
        "{SCRIPT}\nawait new Promise(resolve => window.psaInteractions.{kind}({}, {options}, value => dioxus.send(value), resolve));",
        json!(token),
    );
    spawn(async move {
        // Keep the evaluator alive until the browser disposes this registration.
        let mut eval = document::eval(&script);
        while let Ok(commit) = eval.recv::<Commit>().await {
            if !commit.value.is_finite() || commit.revision <= revision.get() {
                continue;
            }
            revision.set(commit.revision);
            oncommit.call(commit.value);
        }
    });
}

fn use_control_updates(token: &str, options: &str, revision: Rc<Cell<u64>>) {
    // Snapshot the acknowledgement with these props, not when the deferred
    // effect runs: another commit may arrive in between rendering and effects.
    let acknowledged = revision.get();
    use_effect(use_reactive(
        (&token.to_owned(), &options.to_owned(), &acknowledged),
        move |(token, options, acknowledged)| {
            document::eval(&format!(
                "window.psaInteractions?.update({}, {options}, {acknowledged});",
                json!(token),
            ));
        },
    ));
}

/// A native, uncontrolled range whose live value and CSS preview stay in the
/// browser. `value` is the last committed/external value, not an input mirror.
/// Preview properties are set on the nearest `.shell` and removed on unmount.
#[component]
pub(crate) fn RangeInput(
    id: String,
    value: f64,
    min: f64,
    max: f64,
    aria_label: String,
    oncommit: EventHandler<f64>,
    #[props(default = 1.0)] step: f64,
    #[props(default)] preview_property: Option<String>,
    #[props(default)] preview_unit: String,
) -> Element {
    let (token, revision) = use_control();
    // Never render a controlled live value: an unrelated render (or delayed
    // commit acknowledgement) must not reset a thumb currently being dragged.
    let initial = use_hook(|| value.to_string());
    let options = json!({
        "id": id, "value": value, "min": min, "max": max,
        "preview_property": preview_property, "preview_unit": preview_unit,
    })
    .to_string();
    use_control_updates(&token, &options, revision.clone());

    rsx! {
        input {
            id,
            r#type: "range",
            min: min.to_string(),
            max: max.to_string(),
            step: step.to_string(),
            initial_value: initial,
            "aria-label": aria_label,
            onmounted: move |_| mount_control("range", &token, &options, revision.clone(), oncommit),
            // Headless renderers have no DOM/eval. In a browser, native capture
            // listeners stop these events before Dioxus serializes them.
            oninput: move |event| {
                if let Ok(value) = event.value().parse::<f64>()
                    && value.is_finite()
                {
                    oncommit.call(value.clamp(min, max));
                }
            },
            onchange: move |event| {
                if let Ok(value) = event.value().parse::<f64>()
                    && value.is_finite()
                {
                    oncommit.call(value.clamp(min, max));
                }
            },
        }
    }
}

/// Mount directly between the two `.branch` children of the `.split` whose DOM
/// id is `split_id`. The path marker identifies this branch without selectors.
#[component]
pub(crate) fn SplitDivider(
    split_id: String,
    path: Vec<bool>,
    axis: SplitAxis,
    ratio: f64,
    oncommit: EventHandler<f64>,
) -> Element {
    let (token, revision) = use_control();
    let vertical = axis == SplitAxis::Vertical;
    let marker: String = path
        .iter()
        .map(|side| if *side { '1' } else { '0' })
        .collect();
    let options: Value = json!({
        "id": token, "split_id": split_id, "path": marker,
        "axis": if vertical { "vertical" } else { "horizontal" },
        "value": ratio, "min": 0.1, "max": 0.9,
    });
    let options = options.to_string();
    use_control_updates(&token, &options, revision.clone());
    let element_id = token.clone();

    rsx! {
        div {
            id: element_id,
            class: "divider",
            role: "separator",
            tabindex: 0,
            title: "Drag to resize split; arrow keys also resize",
            "data-split-path": marker,
            "aria-label": "Resize split",
            "aria-orientation": if vertical { "horizontal" } else { "vertical" },
            "aria-valuemin": "10",
            "aria-valuemax": "90",
            "aria-valuenow": (ratio * 100.0).round().to_string(),
            onmounted: move |_| mount_control("divider", &token, &options, revision.clone(), oncommit),
            onkeydown: move |event| {
                if event.modifiers().intersects(Modifiers::ALT | Modifiers::CONTROL | Modifiers::META) {
                    return;
                }
                let step = if event.modifiers().contains(Modifiers::SHIFT) { 0.1 } else { 0.02 };
                let next = match event.key() {
                    Key::Home => 0.1,
                    Key::End => 0.9,
                    Key::ArrowLeft if !vertical => ratio - step,
                    Key::ArrowRight if !vertical => ratio + step,
                    Key::ArrowUp if vertical => ratio - step,
                    Key::ArrowDown if vertical => ratio + step,
                    _ => return,
                };
                event.prevent_default();
                event.stop_propagation();
                oncommit.call(next.clamp(0.1, 0.9));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::{
        core::{ElementId, Mutation},
        html::{Code, Location, PlatformEventData, SerializedFormData, SerializedKeyboardData},
    };
    use std::any::Any;

    fn range_fixture() -> Element {
        let mut value = use_signal(|| 14.0);
        rsx! {
            RangeInput {
                id: "test-range", value: value(), min: 11.0, max: 22.0,
                aria_label: "Transcript font size",
                oncommit: move |next| value.set(next),
            }
            output { "{value}" }
        }
    }

    fn divider_fixture() -> Element {
        let mut ratio = use_signal(|| 0.5);
        rsx! {
            div { id: "test-split", class: "split",
                div { class: "branch" }
                SplitDivider {
                    split_id: "test-split", path: vec![true, false],
                    axis: SplitAxis::Horizontal, ratio: ratio(),
                    oncommit: move |next| ratio.set(next),
                }
                div { class: "branch" }
            }
            output { "{ratio}" }
        }
    }

    fn mount(root: fn() -> Element, event_name: &str) -> (VirtualDom, ElementId) {
        dioxus::html::set_event_converter(Box::new(dioxus::html::SerializedHtmlEventConverter));
        let mut dom = VirtualDom::new(root);
        let mutations = dom.rebuild_to_vec();
        let id = mutations
            .edits
            .iter()
            .find_map(|edit| match edit {
                Mutation::NewEventListener { name, id } if name == event_name => Some(*id),
                _ => None,
            })
            .expect("fallback listener");
        (dom, id)
    }

    #[test]
    fn range_has_native_accessibility_and_headless_semantic_fallback() {
        let (mut dom, id) = mount(range_fixture, "input");
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("type=\"range\""), "{html}");
        assert!(
            html.contains("aria-label=\"Transcript font size\""),
            "{html}"
        );
        let event = Event::new(
            Rc::new(PlatformEventData::new(Box::new(SerializedFormData::new(
                "18".into(),
                vec![],
            )))) as Rc<dyn Any>,
            true,
        );
        dom.runtime().handle_event("input", event, id);
        dom.render_immediate_to_vec();
        assert!(dioxus_ssr::render(&dom).contains("<output>18</output>"));
    }

    #[test]
    fn divider_keyboard_fallback_uses_current_ratio_and_exposes_axis() {
        let (mut dom, id) = mount(divider_fixture, "keydown");
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("aria-orientation=\"vertical\""), "{html}");
        assert!(html.contains("data-split-path=\"10\""), "{html}");
        for key in [Key::ArrowRight, Key::ArrowRight, Key::End] {
            let data = SerializedKeyboardData::new(
                key,
                Code::Unidentified,
                Location::Standard,
                false,
                Modifiers::empty(),
                false,
            );
            let event = Event::new(
                Rc::new(PlatformEventData::new(Box::new(data))) as Rc<dyn Any>,
                true,
            );
            dom.runtime().handle_event("keydown", event, id);
            dom.render_immediate_to_vec();
        }
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("aria-valuenow=\"90\""), "{html}");
        assert!(html.contains("<output>0.9</output>"), "{html}");
    }
}
