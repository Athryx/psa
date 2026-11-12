// Browser-owned text. Rust receives snapshots, never ordinary value echoes.
// Kept dependency-free so the exact implementation runs in the Node regressions.
if (!window.psaInputs) {
    const editors = new Map();
    const groups = new Map();
    let clock = 0;
    let disconnectObserver = null;
    const observeDisconnects = () => {
        if (disconnectObserver || typeof MutationObserver === 'undefined') return;
        disconnectObserver = new MutationObserver(() => {
            for (const editor of editors.values()) if (!editor.node.isConnected) editor.dispose();
        });
        disconnectObserver.observe(document.documentElement, { childList: true, subtree: true });
    };

    const selection = node => ({
        start: node.selectionStart ?? 0,
        end: node.selectionEnd ?? 0,
        direction: node.selectionDirection ?? 'none',
    });
    const setSelection = (node, range) => {
        try { node.setSelectionRange(range.start, range.end, range.direction); }
        catch (_) { /* Some input types do not support a text selection. */ }
    };
    const moveOffset = (offset, before, after) => {
        let prefix = 0;
        while (prefix < before.length && prefix < after.length && before[prefix] === after[prefix]) prefix++;
        let suffix = 0;
        while (suffix < before.length - prefix && suffix < after.length - prefix &&
               before[before.length - 1 - suffix] === after[after.length - 1 - suffix]) suffix++;
        if (offset <= prefix) return offset;
        if (offset >= before.length - suffix) return Math.max(prefix, offset + after.length - before.length);
        return after.length - suffix;
    };
    const mirror = editor => {
        if (editor.mirror) editor.mirror.textContent = editor.node.value + ' ';
    };
    const replace = (editor, text, range) => {
        const node = editor.node;
        const before = node.value;
        const previous = selection(node);
        const scrollTop = node.scrollTop;
        const scrollLeft = node.scrollLeft;
        if (before !== text) node.value = text;
        setSelection(node, range ?? {
            start: moveOffset(previous.start, before, text),
            end: moveOffset(previous.end, before, text),
            direction: previous.direction,
        });
        node.scrollTop = scrollTop;
        node.scrollLeft = scrollLeft;
        mirror(editor);
    };
    const snapshot = (editor, cause, response = null) => ({
        revision: { mount: editor.mount, sequence: editor.sequence, clock: editor.group.clock },
        text: editor.node.value,
        selection: selection(editor.node),
        composing: editor.composing,
        applied: editor.group.applied,
        response,
        cause,
    });
    const selectionStamp = editor => {
        const range = selection(editor.node);
        return `${range.start}:${range.end}:${range.direction}:${editor.group.clock}:${editor.group.applied}`;
    };
    const send = (editor, cause, response = null) => {
        if (editor.disposed) return;
        editor.lastSelection = selectionStamp(editor);
        editor.send({ kind: 'snapshot', snapshot: snapshot(editor, cause, response) });
    };
    const publish = (editor, cause) => {
        const group = editor.group;
        editor.sequence++;
        group.mounts.set(editor.mount, { sequence: editor.sequence, composing: editor.composing });
        group.clock = ++clock;
        group.text = editor.node.value;
        for (const other of group.editors) {
            // Never assign the source's value, including during composition.
            if (other !== editor && !other.composing) {
                replace(other, group.text);
                // Shared text edits also invalidate that pane's local command
                // dismissal/selection state, not just its rendered text.
                send(other, cause);
            } else mirror(other);
        }
        send(editor, cause);
    };
    const observe = editor => {
        // Autofill and integrations can change a value before dispatching input.
        // Read at command time, not from a previous render or event closure.
        if (editor.node.value !== editor.group.text) publish(editor, 'edit');
    };
    const mutate = (editor, mutation) => {
        observe(editor);
        const group = editor.group;
        const origin = editors.get(mutation.expected.mount);
        const newer = mutation.id > group.seen;
        group.seen = Math.max(group.seen, mutation.id);
        const composing = [...group.editors].some(other => other.composing);
        if (newer && origin?.group === group && !origin.disposed && !composing &&
            origin.sequence === mutation.expected.sequence && group.clock === mutation.expected.clock) {
            group.text = mutation.text;
            group.applied = mutation.id;
            for (const other of group.editors) {
                const range = other === origin ? (mutation.selection ?? {
                    start: mutation.text.length, end: mutation.text.length, direction: 'none',
                }) : undefined;
                replace(other, group.text, range);
            }
            if (mutation.focus) origin.node.focus({ preventScroll: true });
        }
        // Rejections are snapshots too: Rust must restore its optimistic projection.
        send(editor, 'mutation', mutation.id);
    };

    window.psaInputs = {
        // Call only after the old profile/connection DOM has been removed.
        // Closed drafts in the current generation otherwise retain unseen edits.
        pruneDisconnectedGroups(prefix = '') {
            for (const [key, group] of groups) {
                if (!key.startsWith(prefix)) continue;
                for (const editor of group.editors) if (!editor.node.isConnected) editor.dispose();
                if (group.editors.size === 0) groups.delete(key);
            }
        },
        attach(config, sendPacket) {
            const node = document.querySelector(`[data-psa-input="${config.mount}"]`);
            if (!node) return { dispose() {}, mutate() {} };
            const key = config.draft_key || `mount:${config.mount}`;
            let group = groups.get(key);
            const freshGroup = !group;
            if (!group) {
                group = { text: node.value, clock: ++clock, applied: 0, seen: 0, editors: new Set(), mounts: new Map() };
                groups.set(key, group);
            }
            const fallbackSequence = config.fallback_sequence ?? 0;
            const unseenPreattachEdit = typeof config.fallback_text === 'string'
                ? node.value !== config.fallback_text
                : typeof config.initial_value === 'string' && node.value !== config.initial_value;
            const preattachEdit = unseenPreattachEdit || fallbackSequence > 0;
            if (preattachEdit) {
                group.text = node.value;
                group.clock = ++clock;
                for (const other of group.editors) if (!other.composing) replace(other, group.text);
            }
            const initialMutation = config.mutation;
            const bootstrapMutation = initialMutation && initialMutation.expected.mount === config.mount &&
                initialMutation.expected.clock === 0 && initialMutation.expected.sequence === fallbackSequence &&
                node.value === config.fallback_text && (freshGroup || fallbackSequence > 0) &&
                ![...group.editors].some(other => other.composing);
            if (bootstrapMutation && initialMutation.id > group.seen) {
                group.seen = initialMutation.id;
                group.text = initialMutation.text;
                group.applied = initialMutation.id;
                for (const other of group.editors) replace(other, group.text);
            } else if (initialMutation && !preattachEdit && group.editors.size === 0) {
                const previous = group.mounts.get(initialMutation.expected.mount);
                const newer = initialMutation.id > group.seen;
                group.seen = Math.max(group.seen, initialMutation.id);
                if (newer && previous && !previous.composing &&
                    previous.sequence === initialMutation.expected.sequence &&
                    group.clock === initialMutation.expected.clock) {
                    // Apply a send result that arrived with no editor mounted,
                    // before initializing its replacement (never to a new mount).
                    group.text = initialMutation.text;
                    group.applied = initialMutation.id;
                }
            }
            const editor = {
                node, mount: config.mount, group,
                sequence: fallbackSequence + (unseenPreattachEdit ? 1 : 0), composing: false,
                mirror: document.querySelector(`[data-psa-mirror="${config.mount}"]`),
                send: sendPacket, disposed: false, dismissed: null,
            };
            editors.get(config.mount)?.dispose();
            editors.set(config.mount, editor);
            group.editors.add(editor);
            group.mounts.set(editor.mount, { sequence: editor.sequence, composing: false });
            replace(editor, group.text);

            const input = event => {
                event.stopPropagation();
                publish(editor, 'edit');
            };
            const compositionStart = event => {
                event.stopPropagation();
                editor.composing = true;
                publish(editor, 'composition');
            };
            const compositionEnd = event => {
                event.stopPropagation();
                editor.composing = false;
                publish(editor, 'composition');
            };
            const selected = () => {
                if (editor.disposed || editor.composing || document.activeElement !== node) return;
                observe(editor);
                if (editor.lastSelection !== selectionStamp(editor)) send(editor, 'selection');
            };
            const keydown = event => {
                // Safari may report isComposing=false for the IME commit Enter.
                if (editor.composing || event.isComposing || event.keyCode === 229) {
                    event.stopPropagation();
                    return;
                }
                const composer = node.dataset.psaComposer === 'true';
                const captureEnter = composer || node.dataset.psaCaptureEnter === 'true';
                const captureEscape = composer || node.dataset.psaCaptureEscape === 'true';
                const dismissed = editor.dismissed === group.clock;
                const slash = composer && !dismissed && node.value.startsWith('/') && !node.value.startsWith('//');
                // Names and picker capability come from Rust's COMMANDS, not a
                // duplicated list or a previous render's menu. Match Rust's
                // char::is_whitespace, not JS's different whitespace class.
                const prefix = slash ? node.value.slice(1) : '';
                const delimiter = prefix.search(/[\u0009-\u000d\u0020\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]/u);
                const commands = config.commands ?? [];
                const completion = slash && delimiter === -1 && commands.some(command => command.name.startsWith(prefix));
                const picker = slash && delimiter !== -1 && commands.some(command =>
                    command.picker && command.name === prefix.slice(0, delimiter));
                // A root-model chooser can be open with empty text. A stale
                // menu render must not capture arrows in newly typed plain text.
                const rootChooser = composer && !dismissed && node.dataset.psaMenu === 'true' && node.value === '';
                const menu = completion || picker || rootChooser;
                const plain = !event.ctrlKey && !event.altKey && !event.metaKey;
                const key = event.key;
                const workspaceShortcut = !event.shiftKey && !event.metaKey && (
                    (event.ctrlKey && !event.altKey && (key === 'End' || key.toLowerCase() === 'w')) ||
                    (event.altKey && !event.ctrlKey && (
                        ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(key) ||
                        /^Digit[1-9]$/.test(event.code ?? '')
                    ))
                );
                if (workspaceShortcut || (key === 'Escape' && !captureEscape)) return;
                const handled = (captureEnter && key === 'Enter' && !event.shiftKey) ||
                    (captureEscape && key === 'Escape') ||
                    (plain && !event.shiftKey && (completion || picker) && key === 'Tab') ||
                    (plain && menu && (key === 'ArrowUp' || key === 'ArrowDown'));
                if (!handled) {
                    // The desktop's delegated Dioxus key listener uses synchronous
                    // IPC. Native typing/navigation must never reach that bridge.
                    event.stopPropagation();
                    return;
                }
                // Browser defaults must be stopped here, before an IPC roundtrip.
                event.preventDefault();
                event.stopPropagation();
                observe(editor);
                if (composer && key === 'Escape' && menu) {
                    editor.dismissed = group.clock;
                }
                sendPacket({
                    kind: 'key', snapshot: snapshot(editor, 'key'), key,
                    shift: !!event.shiftKey, control: !!event.ctrlKey,
                    alt: !!event.altKey, meta: !!event.metaKey,
                });
            };
            const submitButton = node.dataset.psaComposer === 'true'
                ? node.closest?.('.composer')?.querySelector('.composer-send') : null;
            const submitted = event => {
                if (node.disabled || submitButton.disabled) return;
                event.preventDefault();
                event.stopPropagation();
                if ([...group.editors].some(other => other.composing)) return;
                observe(editor);
                sendPacket({ kind: 'submit', snapshot: snapshot(editor, 'submit') });
            };
            submitButton?.addEventListener('click', submitted, true);
            const listeners = [
                ['input', input], ['compositionstart', compositionStart], ['compositionend', compositionEnd],
                ['keydown', keydown], ['select', selected], ['keyup', selected], ['pointerup', selected],
            ];
            for (const [name, listener] of listeners) node.addEventListener(name, listener, true);
            document.addEventListener('selectionchange', selected);
            editor.dispose = () => {
                if (editor.disposed) return;
                editor.disposed = true;
                for (const [name, listener] of listeners) node.removeEventListener(name, listener, true);
                document.removeEventListener('selectionchange', selected);
                submitButton?.removeEventListener('click', submitted, true);
                group.editors.delete(editor);
                if (editors.get(config.mount) === editor) editors.delete(config.mount);
                if (editors.size === 0) {
                    disconnectObserver?.disconnect();
                    disconnectObserver = null;
                }
                if (!config.draft_key && group.editors.size === 0) groups.delete(key);
            };
            observeDisconnects();
            send(editor, 'mount', initialMutation?.id ?? null);
            return { dispose: editor.dispose, mutate: mutation => mutate(editor, mutation) };
        },
    };
}
