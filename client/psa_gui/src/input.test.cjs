const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture({ withObserver = false } = {}) {
    const nodes = new Map();
    const mirrors = new Map();
    const documentListeners = new Map();
    const document = {
        activeElement: null,
        documentElement: {},
        querySelector(selector) {
            const mount = Number(selector.match(/="(\d+)"/)?.[1]);
            return selector.includes('data-psa-mirror') ? mirrors.get(mount) : nodes.get(mount);
        },
        addEventListener(name, listener) {
            if (!documentListeners.has(name)) documentListeners.set(name, new Set());
            documentListeners.get(name).add(listener);
        },
        removeEventListener(name, listener) { documentListeners.get(name)?.delete(listener); },
    };
    const window = {};
    const observers = [];
    const context = { window, document };
    if (withObserver) context.MutationObserver = class {
        constructor(callback) { this.callback = callback; this.connected = false; observers.push(this); }
        observe() { this.connected = true; }
        disconnect() { this.connected = false; }
    };
    vm.runInNewContext(fs.readFileSync(path.join(__dirname, 'input.js'), 'utf8'), context);
    let nextMount = 1;
    const create = (text = '', key = '', options = {}, installation = {}) => {
        const mount = nextMount++;
        const listeners = new Map();
        const buttonListeners = new Map();
        const packets = [];
        const button = {
            disabled: false,
            addEventListener(name, listener) { buttonListeners.set(name, listener); },
            removeEventListener(name, listener) { if (buttonListeners.get(name) === listener) buttonListeners.delete(name); },
        };
        const node = {
            _value: text, writes: 0, isConnected: true,
            selectionStart: text.length, selectionEnd: text.length, selectionDirection: 'none',
            scrollTop: 0, scrollLeft: 0, disabled: false,
            closest: () => ({ querySelector: () => button }),
            dataset: { psaComposer: 'true', psaMenu: 'false', ...options },
            get value() { return this._value; },
            set value(value) { this.writes++; this._value = value; this.selectionStart = this.selectionEnd = value.length; },
            setSelectionRange(start, end, direction) {
                this.selectionStart = Math.max(0, Math.min(start, this.value.length));
                this.selectionEnd = Math.max(0, Math.min(end, this.value.length));
                this.selectionDirection = direction;
            },
            focus() { document.activeElement = this; },
            addEventListener(name, listener) { listeners.set(name, listener); },
            removeEventListener(name, listener) { if (listeners.get(name) === listener) listeners.delete(name); },
        };
        const mirror = { textContent: '' };
        nodes.set(mount, node);
        mirrors.set(mount, mirror);
        installation.beforeAttach?.(node);
        const controller = window.psaInputs.attach({
            mount, draft_key: key, initial_value: text, fallback_text: text,
            commands: [
                { name: 'model', picker: true }, { name: 'effort', picker: true }, { name: 'info', picker: false },
            ],
            ...installation.config,
        }, packet => {
            // Simulate IPC serialization, not shared mutable object references.
            packets.push(JSON.parse(JSON.stringify(packet)));
        });
        const dispatchTo = (handlers, name, properties = {}) => {
            const event = {
                key: '', shiftKey: false, ctrlKey: false, altKey: false, metaKey: false,
                defaultPrevented: false, stopped: false,
                preventDefault() { this.defaultPrevented = true; },
                stopPropagation() { this.stopped = true; },
                ...properties,
            };
            handlers.get(name)?.(event);
            return event;
        };
        const dispatch = (name, properties) => dispatchTo(listeners, name, properties);
        const click = () => dispatchTo(buttonListeners, 'click');
        const edit = (text, start = text.length, end = start, direction = 'none') => {
            // A native browser edit is not a programmatic .value assignment.
            node._value = text;
            node.setSelectionRange(start, end, direction);
            dispatch('input');
        };
        return {
            mount, node, mirror, packets, controller, dispatch, edit, listeners, click, button, buttonListeners,
            last: () => packets.at(-1),
            mutation: (snapshot, text, id = 1, extra = {}) => ({
                id, expected: snapshot.revision, text, selection: null, focus: false, ...extra,
            }),
        };
    };
    return { create, document, window, documentListeners, observers };
}

test('an old abc frame cannot echo over ac; mirror and caret update before IPC', () => {
    const { create } = fixture();
    const input = create('ab');
    input.edit('abc');
    const oldFrame = input.last().snapshot;
    input.edit('ac', 1);
    assert.equal(input.mirror.textContent, 'ac ');
    assert.equal(input.node.selectionStart, 1);
    assert.equal(input.node.writes, 0, 'ordinary edits must not write source.value');
    input.controller.mutate(input.mutation(oldFrame, 'abc'));
    assert.equal(input.node.value, 'ac');
    assert.equal(input.node.selectionStart, 1);
    assert.equal(input.last().snapshot.text, 'ac');
    assert.equal(input.node.writes, 0);
});

test('delayed send clear cannot delete a newer edit, including same-text ABA edits', () => {
    const { create } = fixture();
    const input = create('send me');
    const submitted = input.last().snapshot;
    input.edit('send me plus newer text');
    input.controller.mutate(input.mutation(submitted, '', 1));
    assert.equal(input.node.value, 'send me plus newer text');
    assert.equal(input.last().snapshot.response, 1);
    assert.equal(input.last().snapshot.applied, 0);
    input.edit('send me');
    input.controller.mutate(input.mutation(submitted, '', 2));
    assert.equal(input.node.value, 'send me');
    assert.equal(input.last().snapshot.applied, 0);
});

test('native paste replaces the selected range without a Rust echo or lost caret', () => {
    const { create } = fixture();
    const input = create('abcd');
    input.node.setSelectionRange(1, 3, 'forward');
    input.edit('aXYZd', 4);
    assert.equal(input.node.value, 'aXYZd');
    assert.equal(input.node.selectionStart, 4);
    assert.equal(input.node.selectionEnd, 4);
    assert.equal(input.node.writes, 0);
    assert.equal(input.mirror.textContent, 'aXYZd ');
    assert.deepEqual(input.last().snapshot.selection, { start: 4, end: 4, direction: 'none' });
});

test('intentional replacement uses UTF-16 selections and successful clear is acknowledged', () => {
    const { create } = fixture();
    const input = create('/mo');
    const initial = input.last().snapshot;
    input.controller.mutate(input.mutation(initial, 'a😀b', 1, {
        selection: { start: 1, end: 3, direction: 'backward' },
    }));
    assert.equal(input.node.value, 'a😀b');
    assert.equal(input.node.selectionStart, 1);
    assert.equal(input.node.selectionEnd, 3);
    assert.equal(input.node.selectionDirection, 'backward');
    assert.equal(input.last().snapshot.applied, 1);
    input.controller.mutate(input.mutation(input.last().snapshot, '', 2));
    assert.equal(input.node.value, '');
    assert.equal(input.node.selectionStart, 0);
    assert.equal(input.mirror.textContent, ' ');
    assert.equal(input.last().snapshot.response, 2);
    assert.equal(input.last().snapshot.applied, 2);
});

test('IME edits grow immediately; shortcuts and mutations cannot disturb composition', () => {
    const { create } = fixture();
    const input = create('');
    const beforeComposition = input.last().snapshot;
    input.dispatch('compositionstart');
    input.edit('に\nほん');
    assert.equal(input.mirror.textContent, 'に\nほん ');
    input.controller.mutate(input.mutation(beforeComposition, 'wrong', 1));
    assert.equal(input.node.value, 'に\nほん');
    const packetCount = input.packets.length;
    const composingEnter = input.dispatch('keydown', { key: 'Enter', isComposing: false });
    assert.equal(input.packets.length, packetCount);
    assert.equal(composingEnter.defaultPrevented, false, 'IME retains its native commit action');
    input.dispatch('compositionend');
    const safariEnter = input.dispatch('keydown', { key: 'Enter', keyCode: 229, isComposing: false });
    assert.equal(safariEnter.defaultPrevented, false);
    assert.equal(input.packets.filter(packet => packet.kind === 'key').length, 0);
    input.dispatch('keydown', { key: 'Enter' });
    assert.equal(input.last().kind, 'key');
    assert.equal(input.last().snapshot.text, 'に\nほん');
    assert.equal(input.node.writes, 0);
});

test('keydown snapshots the live text/selection, not the previous rendered command menu', () => {
    const { create } = fixture();
    const input = create('');
    input.edit('/mo', 2);
    const tab = input.dispatch('keydown', { key: 'Tab' });
    assert.equal(tab.defaultPrevented, true, 'new slash command works before Rust renders its popup');
    assert.equal(input.last().snapshot.text, '/mo');
    assert.equal(input.last().snapshot.selection.start, 2);
    input.edit('plain text');
    input.node.dataset.psaMenu = 'true'; // A delayed render still thinks the popup is visible.
    const arrow = input.dispatch('keydown', { key: 'ArrowUp' });
    assert.equal(arrow.defaultPrevented, false);
    const before = input.packets.length;
    const shifted = input.dispatch('keydown', { key: 'Enter', shiftKey: true });
    assert.equal(shifted.defaultPrevented, false);
    assert.equal(input.packets.length, before);
});

test('literal slash text keeps native Tab/caret navigation; command pickers capture Tab and arrows', () => {
    const { create } = fixture();
    for (const text of ['/unknown', '/usr/bin', '/select agent', '/select agent worker', '/refresh', '/info arguments', '//model']) {
        const input = create(text);
        for (const key of ['Tab', 'ArrowUp', 'ArrowDown']) {
            const event = input.dispatch('keydown', { key });
            assert.equal(event.defaultPrevented, false, `${text}: ${key}`);
            assert.equal(event.stopped, true);
        }
        assert.equal(input.packets.filter(packet => packet.kind === 'key').length, 0);
    }
    for (const text of ['/model ', '/effort high', '/model\u0085search']) {
        const input = create(text);
        assert.equal(input.dispatch('keydown', { key: 'Tab' }).defaultPrevented, true, text);
        assert.equal(input.dispatch('keydown', { key: 'ArrowUp' }).defaultPrevented, true, text);
    }
    const root = create('/select agent worker', '', {}, { config: { commands: [{ name: 'select agent', picker: true }] } });
    assert.equal(root.dispatch('keydown', { key: 'Tab' }).defaultPrevented, true);
    assert.equal(root.dispatch('keydown', { key: 'ArrowDown' }).defaultPrevented, true);
    for (const text of ['/sel', '/select ', '/select a']) {
        root.edit(text);
        assert.equal(root.dispatch('keydown', { key: 'Tab' }).defaultPrevented, true, text);
    }
    const chooser = create('', '', { psaMenu: 'true' });
    assert.equal(chooser.dispatch('keydown', { key: 'Tab' }).defaultPrevented, false);
    assert.equal(chooser.dispatch('keydown', { key: 'ArrowUp' }).defaultPrevented, true);
});

test('selection snapshots are deduplicated without delaying a changed range', () => {
    const { create } = fixture();
    const input = create('abcdef');
    input.node.focus();
    input.edit('abcdefg');
    const afterEdit = input.packets.length;
    input.dispatch('keyup');
    input.dispatch('select');
    assert.equal(input.packets.length, afterEdit);
    input.node.setSelectionRange(1, 5, 'backward');
    input.dispatch('select');
    assert.equal(input.packets.length, afterEdit + 1);
    assert.equal(input.last().snapshot.cause, 'selection');
    input.dispatch('keyup');
    assert.equal(input.packets.length, afterEdit + 1);
});

test('peer edits expire local command dismissal and notify the other pane state', () => {
    const { create } = fixture();
    const left = create('/mo', 'duplicate-dismissal');
    const right = create('/mo', 'duplicate-dismissal');
    left.dispatch('keydown', { key: 'Escape' });
    assert.equal(left.dispatch('keydown', { key: 'Tab' }).defaultPrevented, false);
    right.edit('/mod');
    assert.equal(left.last().snapshot.cause, 'edit');
    assert.equal(left.last().snapshot.text, '/mod');
    assert.equal(left.dispatch('keydown', { key: 'Tab' }).defaultPrevented, true);
});

test('peer intentional replacement does not resurrect a locally dismissed popup', () => {
    const { create } = fixture();
    const left = create('/mo', 'duplicate-intent-dismissal');
    const right = create('/mo', 'duplicate-intent-dismissal');
    left.dispatch('keydown', { key: 'Escape' });
    right.controller.mutate(right.mutation(right.last().snapshot, '/model ', 1));
    assert.equal(left.node.value, '/model ');
    assert.equal(left.dispatch('keydown', { key: 'Tab' }).defaultPrevented, false);
    assert.equal(left.dispatch('keydown', { key: 'ArrowUp' }).defaultPrevented, false);
    left.edit('/model a');
    assert.equal(left.dispatch('keydown', { key: 'ArrowUp' }).defaultPrevented, true);
});

test('Escape dismissal leaves slash Tab and caret arrows native until the draft changes', () => {
    const { create } = fixture();
    const input = create('/mo');
    input.dispatch('keydown', { key: 'Escape' });
    const afterEscape = input.packets.length;
    for (const key of ['Tab', 'ArrowUp', 'ArrowDown']) {
        const event = input.dispatch('keydown', { key });
        assert.equal(event.defaultPrevented, false);
        assert.equal(event.stopped, true);
    }
    assert.equal(input.packets.length, afterEscape);
    input.edit('/mod');
    assert.equal(input.dispatch('keydown', { key: 'Tab' }).defaultPrevented, true);
});

test('pointer Send carries the freshest snapshot on the same ordered edit channel', () => {
    const { create } = fixture();
    const input = create('old Rust snapshot');
    input.edit('pasted just before click', 6, 12, 'backward');
    const clicked = input.click();
    assert.equal(clicked.defaultPrevented, true);
    assert.equal(clicked.stopped, true, 'native Dioxus onclick must not submit a duplicate');
    assert.equal(input.last().kind, 'submit');
    assert.equal(input.last().snapshot.text, 'pasted just before click');
    assert.deepEqual(input.last().snapshot.selection, { start: 6, end: 12, direction: 'backward' });
    input.dispatch('compositionstart');
    const before = input.packets.length;
    assert.equal(input.click().stopped, true);
    assert.equal(input.packets.length, before, 'a click must not submit a partial IME composition');
});

test('single-line action fields bridge opted-in Enter/Escape, never ordinary typing', () => {
    const { create } = fixture();
    const input = create('path', '', {
        psaComposer: 'false', psaCaptureEnter: 'true', psaCaptureEscape: 'true',
    });
    const before = input.packets.length;
    const letter = input.dispatch('keydown', { key: 'a' });
    assert.equal(letter.stopped, true);
    assert.equal(letter.defaultPrevented, false);
    assert.equal(input.packets.length, before);
    input.dispatch('keydown', { key: 'Enter' });
    assert.equal(input.last().snapshot.text, 'path');
    assert.equal(input.last().key, 'Enter');
    assert.equal(input.dispatch('keydown', { key: 'Escape' }).stopped, true);
});

test('generic fields leave Escape to their modal but do not bridge Enter or native navigation', () => {
    const { create } = fixture();
    const input = create('text', '', { psaComposer: 'false' });
    const before = input.packets.length;
    const escape = input.dispatch('keydown', { key: 'Escape' });
    assert.equal(escape.stopped, false);
    assert.equal(escape.defaultPrevented, false);
    for (const key of ['a', 'Enter', 'ArrowLeft', 'Tab']) {
        const event = input.dispatch('keydown', { key });
        assert.equal(event.stopped, true, key);
        assert.equal(event.defaultPrevented, false, key);
    }
    assert.equal(input.packets.length, before);
});

test('only exact workspace shortcuts propagate through the synchronous Dioxus key bridge', () => {
    const { create } = fixture();
    const input = create('/mo');
    for (const properties of [
        { key: 'w', ctrlKey: true }, { key: 'End', ctrlKey: true },
        { key: 'ArrowRight', altKey: true }, { key: '1', code: 'Digit1', altKey: true },
    ]) {
        const event = input.dispatch('keydown', properties);
        assert.equal(event.stopped, false);
        assert.equal(event.defaultPrevented, false);
    }
    for (const properties of [
        { key: 'w', ctrlKey: true, shiftKey: true }, { key: 'End', ctrlKey: true, altKey: true },
        { key: 'ArrowRight', altKey: true, metaKey: true }, { key: 'a', ctrlKey: true },
    ]) {
        const event = input.dispatch('keydown', properties);
        assert.equal(event.stopped, true);
        assert.equal(event.defaultPrevented, false);
    }
});

test('duplicate panes share browser text without ever echoing into the source editor', () => {
    const { create } = fixture();
    const left = create('abc', 'composer:1:2:3');
    const right = create('stale initial value', 'composer:1:2:3');
    assert.equal(right.node.value, 'abc');
    right.node.setSelectionRange(1, 2, 'backward');
    left.edit('abcd');
    assert.equal(left.node.writes, 0);
    assert.equal(right.node.value, 'abcd');
    assert.equal(right.node.selectionStart, 1);
    assert.equal(right.node.selectionEnd, 2);
    assert.equal(right.node.selectionDirection, 'backward');
    assert.equal(right.mirror.textContent, 'abcd ');
    const submitted = left.last().snapshot;
    right.edit('right side typed');
    assert.equal(left.node.value, 'right side typed');
    assert.equal(right.last().snapshot.revision.sequence, 1);
    assert.ok(right.last().snapshot.revision.clock > submitted.revision.clock);
    left.controller.mutate(left.mutation(submitted, '', 1));
    assert.equal(left.node.value, 'right side typed');
    assert.equal(right.node.value, 'right side typed');
    right.controller.mutate(right.mutation(right.last().snapshot, '', 2));
    assert.equal(left.node.value, '');
    assert.equal(right.node.value, '');
});

test('a stale mount cannot mutate a replacement mount with a reset sequence', () => {
    const { create } = fixture();
    const old = create('retained', 'draft');
    const oldSnapshot = old.last().snapshot;
    old.controller.dispose();
    const current = create('stale Rust initial', 'draft');
    assert.equal(current.node.value, 'retained');
    current.controller.mutate(current.mutation(oldSnapshot, '', 1));
    assert.equal(current.node.value, 'retained');
    assert.equal(current.last().snapshot.applied, 0);
});

test('promotion keeps browser edits that have not reached the old Rust draft snapshot', () => {
    const { create } = fixture();
    const old = create('first message', 'stable-draft-identity');
    const staleRustText = old.last().snapshot.text;
    old.edit('first message plus an unseen edit');
    // Promotion transfers the stable key and unmounts before this edit is read by Rust.
    old.controller.dispose();
    const promoted = create(staleRustText, 'stable-draft-identity');
    assert.equal(promoted.node.value, 'first message plus an unseen edit');
    assert.equal(promoted.last().snapshot.text, 'first message plus an unseen edit');
    assert.equal(promoted.mirror.textContent, 'first message plus an unseen edit ');
});

test('a pending send clear applies before reopening only if the retained draft is unchanged', () => {
    const { create } = fixture();
    const old = create('sent', 'stable-closed-draft');
    const clear = old.mutation(old.last().snapshot, '', 1);
    old.controller.dispose();
    const reopened = create('', 'stable-closed-draft', {}, { config: { mutation: clear } });
    assert.equal(reopened.node.value, '');
    assert.equal(reopened.last().snapshot.applied, 1);
    assert.equal(reopened.last().snapshot.response, 1);

    const newer = create('sent', 'another-closed-draft');
    const staleClear = newer.mutation(newer.last().snapshot, '', 2);
    newer.edit('an edit Rust has not read yet');
    newer.controller.dispose();
    const retained = create('', 'another-closed-draft', {}, { config: { mutation: staleClear } });
    assert.equal(retained.node.value, 'an edit Rust has not read yet');
    assert.equal(retained.last().snapshot.applied, 0);
    assert.equal(retained.last().snapshot.response, 2);
});

test('listener installation adopts preattach typing and continues the fallback sequence', () => {
    const { create } = fixture();
    const input = create('initial', '', {}, {
        config: { fallback_sequence: 4, fallback_text: 'last fallback' },
        beforeAttach: node => { node._value = 'latest native text'; },
    });
    assert.equal(input.node.value, 'latest native text');
    assert.equal(input.node.writes, 0);
    assert.equal(input.last().snapshot.revision.sequence, 5);
    assert.ok(input.last().snapshot.revision.clock > 0);
    input.edit('next native edit');
    assert.equal(input.last().snapshot.revision.sequence, 6);
});

test('a guarded fallback command can hand off into a newly installed native editor', () => {
    const { create } = fixture();
    const input = create('', '', {}, {
        config: {
            fallback_sequence: 3, fallback_text: '/mo',
            mutation: { id: 1, expected: { mount: 1, sequence: 3, clock: 0 }, text: '/model ', selection: null, focus: false },
        },
        beforeAttach: node => { node._value = '/mo'; },
    });
    assert.equal(input.node.value, '/model ');
    assert.equal(input.last().snapshot.applied, 1);
    assert.equal(input.last().snapshot.revision.sequence, 3);
});

test('generation pruning removes only disconnected old-namespace groups', () => {
    const { create, window } = fixture();
    const closedOld = create('old retained browser text', 'psa:1:closed');
    const activeOld = create('active old browser text', 'psa:1:active');
    const closedNew = create('new retained browser text', 'psa:2:closed');
    closedOld.controller.dispose();
    closedNew.controller.dispose();
    window.psaInputs.pruneDisconnectedGroups('psa:1:');
    assert.equal(create('fresh seed', 'psa:1:closed').node.value, 'fresh seed');
    assert.equal(create('stale seed', 'psa:1:active').node.value, activeOld.node.value);
    assert.equal(create('stale seed', 'psa:2:closed').node.value, 'new retained browser text');
});

test('older and duplicate intentional mutations cannot reorder a newer replacement', () => {
    const { create } = fixture();
    const input = create('/');
    const base = input.last().snapshot;
    input.controller.mutate(input.mutation(base, '/model ', 2));
    input.controller.mutate(input.mutation(base, '/agent ', 1));
    input.controller.mutate(input.mutation(base, 'duplicate must not apply', 2));
    assert.equal(input.node.value, '/model ');
    assert.equal(input.node.selectionStart, '/model '.length);
    assert.equal(input.last().snapshot.applied, 2);
});

test('one disconnect observer serves all fields and stops when the last editor is removed', () => {
    const { create, observers } = fixture({ withObserver: true });
    const inputs = Array.from({ length: 8 }, (_, index) => create(`field ${index}`));
    assert.equal(observers.length, 1, 'streaming DOM updates must not fan out to one observer per input');
    inputs[0].node.isConnected = false;
    observers[0].callback();
    assert.equal(inputs[0].listeners.size, 0);
    assert.equal(observers[0].connected, true);
    for (const input of inputs.slice(1)) input.controller.dispose();
    assert.equal(observers[0].connected, false);
    create('new mounted field');
    assert.equal(observers.length, 2);
    assert.equal(observers[1].connected, true);
});

test('disposal removes input, selection, and keyboard listeners', () => {
    const { create, documentListeners } = fixture();
    const input = create('draft');
    input.controller.dispose();
    assert.equal(input.listeners.size, 0);
    assert.equal(input.buttonListeners.size, 0);
    assert.equal(documentListeners.get('selectionchange').size, 0);
    const before = input.packets.length;
    input.edit('detached');
    input.dispatch('keydown', { key: 'Enter' });
    assert.equal(input.packets.length, before);
});
