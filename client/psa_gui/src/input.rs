//! Browser-owned text inputs. Ordinary edits only travel from the DOM to Rust.
//! A value travels back only as an explicit, revision-guarded mutation.
use dioxus::{document, prelude::*};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "draft.rs"]
mod draft;
pub(crate) use draft::DraftStore;

static NEXT_MOUNT: AtomicU64 = AtomicU64::new(1);
static NEXT_MUTATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_mutation_id() -> u64 {
    NEXT_MUTATION.fetch_add(1, Ordering::Relaxed)
}

/// `sequence` belongs to one mounted editor; `clock` orders edits across editors.
/// Intentional writes have their own IDs and do not masquerade as user edits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputRevision {
    pub mount: u64,
    pub sequence: u64,
    pub clock: u64,
}

/// Offsets use the browser's UTF-16 code units, not UTF-8 bytes or Rust chars.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputSelection {
    pub start: usize,
    pub end: usize,
    pub direction: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotCause {
    #[default]
    Mount,
    Edit,
    Composition,
    Selection,
    Mutation,
    Key,
    Submit,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputSnapshot {
    pub revision: InputRevision,
    pub text: String,
    pub selection: InputSelection,
    pub composing: bool,
    /// Last successfully applied intentional mutation in this shared draft.
    pub applied: u64,
    /// The mutation being acknowledged, including a rejected mutation.
    pub response: Option<u64>,
    pub cause: SnapshotCause,
}

impl InputSnapshot {
    pub(crate) fn is_edit(&self) -> bool {
        matches!(self.cause, SnapshotCause::Edit | SnapshotCause::Composition)
    }

    pub(crate) fn replacement(&self, text: String) -> InputMutation {
        InputMutation {
            id: next_mutation_id(),
            expected: self.revision,
            text,
            selection: None,
            focus: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputMutation {
    pub id: u64,
    pub expected: InputRevision,
    pub text: String,
    /// None deliberately puts the initiating editor's caret at the end.
    pub selection: Option<InputSelection>,
    pub focus: bool,
}

/// A command always carries the value/selection read at native keydown time.
/// Never select a slash command from a list captured by the previous render.
#[derive(Clone)]
pub(crate) struct InputKeyEvent {
    pub snapshot: InputSnapshot,
    key: Key,
    modifiers: Modifiers,
    fallback: Option<KeyboardEvent>,
}

impl InputKeyEvent {
    #[cfg(test)]
    pub(crate) fn from_snapshot(snapshot: InputSnapshot, key: Key) -> Self {
        Self {
            snapshot,
            key,
            modifiers: Modifiers::empty(),
            fallback: None,
        }
    }

    pub(crate) fn key(&self) -> Key {
        self.key.clone()
    }

    pub(crate) fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub(crate) fn prevent_default(&self) {
        if let Some(event) = &self.fallback {
            event.prevent_default();
        }
    }

    pub(crate) fn stop_propagation(&self) {
        if let Some(event) = &self.fallback {
            event.stop_propagation();
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BrowserPacket {
    Snapshot {
        snapshot: InputSnapshot,
    },
    Submit {
        snapshot: InputSnapshot,
    },
    Key {
        snapshot: InputSnapshot,
        key: String,
        shift: bool,
        control: bool,
        alt: bool,
        meta: bool,
    },
}

impl BrowserPacket {
    fn key_event(
        snapshot: InputSnapshot,
        key: String,
        shift: bool,
        control: bool,
        alt: bool,
        meta: bool,
    ) -> InputKeyEvent {
        let mut modifiers = Modifiers::empty();
        modifiers.set(Modifiers::SHIFT, shift);
        modifiers.set(Modifiers::CONTROL, control);
        modifiers.set(Modifiers::ALT, alt);
        modifiers.set(Modifiers::META, meta);
        InputKeyEvent {
            snapshot,
            key: key.parse().unwrap_or(Key::Unidentified),
            modifiers,
            fallback: None,
        }
    }
}

/// `draft_key` is optional; equal keys intentionally share a browser-owned draft
/// across split panes. Include the connection/profile generation in shared keys.
/// `initial_value` is frozen for this mount, not a controlled `value` attribute.
#[component]
pub(crate) fn BrowserInput(
    #[props(default)] initial_value: String,
    #[props(default)] mutation: Option<InputMutation>,
    #[props(default)] draft_key: String,
    #[props(default)] multiline: bool,
    #[props(default)] autogrow: bool,
    #[props(default)] composer: bool,
    #[props(default)] root_commands: bool,
    #[props(default)] capture_enter: bool,
    #[props(default)] capture_escape: bool,
    #[props(default)] menu_open: bool,
    #[props(default)] disabled: bool,
    #[props(default)] required: bool,
    #[props(default)] aria_label: String,
    #[props(default)] class: String,
    #[props(default)] placeholder: String,
    #[props(default)] id: String,
    #[props(default)] name: String,
    #[props(default = "text".to_owned())] r#type: String,
    #[props(default)] autofocus: bool,
    #[props(default = "off".to_owned())] autocomplete: String,
    #[props(default)] onedit: EventHandler<InputSnapshot>,
    #[props(default)] onkey: EventHandler<InputKeyEvent>,
    #[props(default)] onsubmit: EventHandler<InputSnapshot>,
    #[props(default)] onmounted: EventHandler<MountedEvent>,
) -> Element {
    let mount = use_hook(|| NEXT_MOUNT.fetch_add(1, Ordering::Relaxed));
    let initial = use_hook(|| initial_value);
    let input_id = if id.is_empty() {
        format!("psa-input-{mount}")
    } else {
        id
    };
    let mut evaluator = use_signal(|| None::<document::Eval>);
    let mut native_active = use_signal(|| false);
    let mut sent_mutation = use_signal(|| None::<u64>);
    let mut last_snapshot = use_signal(|| InputSnapshot {
        revision: InputRevision {
            mount,
            ..Default::default()
        },
        text: initial.clone(),
        ..Default::default()
    });

    // Only explicit mutations go to JavaScript. Changing a Rust snapshot never
    // schedules a DOM value assignment (nor an initial_value/defaultValue edit).
    use_effect(use_reactive((&mutation,), move |(mutation,)| {
        let eval = *evaluator.read();
        let active = *native_active.read();
        let Some(mutation) = mutation else {
            return;
        };
        if let Some(eval) = eval {
            if *sent_mutation.peek() != Some(mutation.id)
                && eval
                    .send(serde_json::json!({ "kind": "mutation", "mutation": mutation }))
                    .is_ok()
            {
                sent_mutation.set(Some(mutation.id));
            }
        }
        if active {
            return;
        }
        let mut snapshot = last_snapshot.peek().clone();
        if snapshot.revision != mutation.expected
            || snapshot.composing
            || mutation.id <= snapshot.applied
        {
            return;
        }
        snapshot.text = mutation.text;
        snapshot.applied = mutation.id;
        snapshot.response = Some(mutation.id);
        snapshot.cause = SnapshotCause::Mutation;
        snapshot.selection = mutation.selection.unwrap_or_else(|| {
            let end = snapshot.text.encode_utf16().count();
            InputSelection {
                start: end,
                end,
                direction: "none".into(),
            }
        });
        last_snapshot.set(snapshot.clone());
        onedit.call(snapshot);
    }));
    use_drop(move || {
        if let Some(eval) = *evaluator.peek() {
            let _ = eval.send(serde_json::json!({ "kind": "dispose" }));
        }
    });

    let mount_initial = initial.clone();
    let mounted = move |event: MountedEvent| {
        let commands: Vec<_> = client_shared::COMMANDS
            .iter()
            .filter(|command| command.id.is_available(root_commands))
            .map(|command| serde_json::json!({ "name": command.name, "picker": command.picker }))
            .collect();
        let config = serde_json::json!({
            "commands": commands,
            "mount": mount, "draft_key": draft_key, "initial_value": mount_initial,
            "mutation": mutation, "fallback_sequence": last_snapshot.peek().revision.sequence,
            "fallback_text": last_snapshot.peek().text,
        });
        let mut eval = document::eval(&format!(
            "{}\nconst editor = window.psaInputs.attach({}, packet => dioxus.send(packet));\n\
             try {{ while (true) {{ const message = await dioxus.recv();\n\
             if (message.kind === 'dispose') break;\n\
             if (message.kind === 'mutation') editor.mutate(message.mutation);\n\
             }} }} finally {{ editor.dispose(); }}",
            include_str!("input.js"),
            config,
        ));
        evaluator.set(Some(eval));
        spawn(async move {
            while let Ok(packet) = eval.recv::<BrowserPacket>().await {
                if !*native_active.peek() {
                    native_active.set(true);
                }
                match packet {
                    BrowserPacket::Snapshot { snapshot } => {
                        last_snapshot.set(snapshot.clone());
                        onedit.call(snapshot);
                    }
                    BrowserPacket::Submit { snapshot } => {
                        last_snapshot.set(snapshot.clone());
                        onsubmit.call(snapshot);
                    }
                    BrowserPacket::Key {
                        snapshot,
                        key,
                        shift,
                        control,
                        alt,
                        meta,
                    } => {
                        last_snapshot.set(snapshot.clone());
                        onkey.call(BrowserPacket::key_event(
                            snapshot, key, shift, control, alt, meta,
                        ));
                    }
                }
            }
        });
        onmounted.call(event);
    };
    // Headless VirtualDom tests can still dispatch ordinary Dioxus events.
    // The native capture listeners suppress these events in a real browser.
    let edited = move |event: FormEvent| {
        if *native_active.peek() {
            return;
        }
        let mut snapshot = last_snapshot.peek().clone();
        snapshot.revision.sequence += 1;
        // Clock zero is the bootstrap/headless namespace. Native clocks start
        // above zero and carry this mount's sequence forward on installation.
        snapshot.revision.clock = 0;
        snapshot.text = event.value();
        snapshot.cause = SnapshotCause::Edit;
        snapshot.response = None;
        last_snapshot.set(snapshot.clone());
        onedit.call(snapshot);
    };
    let keyed = move |event: KeyboardEvent| {
        if *native_active.peek() || event.is_composing() {
            return;
        }
        let mut snapshot = last_snapshot.peek().clone();
        snapshot.cause = SnapshotCause::Key;
        onkey.call(InputKeyEvent {
            snapshot,
            key: event.key(),
            modifiers: event.modifiers(),
            fallback: Some(event),
        });
    };

    if multiline {
        let textarea = rsx! {
            textarea {
                id: input_id,
                name: (!name.is_empty()).then_some(name),
                class,
                initial_value: initial.clone(),
                rows: "1",
                disabled,
                required,
                autofocus,
                autocomplete: autocomplete.clone(),
                placeholder: (!placeholder.is_empty()).then_some(placeholder),
                "aria-label": aria_label,
                "data-psa-input": "{mount}",
                "data-psa-composer": composer,
                "data-psa-capture-enter": capture_enter,
                "data-psa-capture-escape": capture_escape,
                "data-psa-menu": menu_open,
                onmounted: mounted,
                oninput: edited,
                onkeydown: keyed,
            }
        };
        if autogrow {
            return rsx! {
                div { class: "composer-grow",
                    // Static initial content is never patched with old Rust text.
                    div { class: "composer-mirror", "aria-hidden": "true", "data-psa-mirror": "{mount}", "{initial} " }
                    {textarea}
                }
            };
        }
        return textarea;
    }

    rsx! {
        input {
            id: input_id,
            name: (!name.is_empty()).then_some(name),
            class,
            r#type,
            initial_value: initial,
            disabled,
            required,
            autofocus,
            autocomplete,
            placeholder: (!placeholder.is_empty()).then_some(placeholder),
            "aria-label": aria_label,
            "data-psa-input": "{mount}",
            "data-psa-composer": composer,
            "data-psa-capture-enter": capture_enter,
            "data-psa-capture-escape": capture_escape,
            "data-psa-menu": menu_open,
            onmounted: mounted,
            oninput: edited,
            onkeydown: keyed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::core::Mutation;

    fn echo_fixture() -> Element {
        let text = use_signal(|| "abc".to_owned());
        use_context_provider(|| text);
        rsx! { BrowserInput { initial_value: text(), multiline: true, autogrow: true } }
    }

    #[test]
    fn rust_frames_never_echo_value_or_mirror_text_after_mount() {
        let mut dom = VirtualDom::new(echo_fixture);
        let initial = dom.rebuild_to_vec();
        assert!(initial.edits.iter().any(|edit| matches!(
            edit,
            Mutation::SetAttribute {
                name: "initial_value",
                ..
            }
        )));
        assert!(
            !initial
                .edits
                .iter()
                .any(|edit| matches!(edit, Mutation::SetAttribute { name: "value", .. }))
        );
        let mut text = dom.in_scope(ScopeId::APP, consume_context::<Signal<String>>);
        for snapshot in ["ac", "abc", "ac"] {
            text.set(snapshot.into());
            let frame = dom.render_immediate_to_vec();
            assert!(
                !frame.edits.iter().any(|edit| matches!(
                    edit,
                    Mutation::SetAttribute {
                        name: "value" | "initial_value",
                        ..
                    } | Mutation::SetText { .. }
                )),
                "a delayed Rust snapshot must not touch browser text: {:?}",
                frame.edits
            );
        }
    }
}
