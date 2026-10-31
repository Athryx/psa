const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, 'pane_focus.js'), 'utf8');

function fixture() {
    const handlers = {}, windowHandlers = {};
    const frames = new Map(), timers = new Map(), observers = new Set();
    const focusCalls = [];
    const shell = { isConnected: true };
    let modal = false, selection = true, current = 1, nextId = 1;
    const panes = [1, 2].map(id => {
        const pane = {
            id, connected: true, activeMounted: true, inputMounted: true, disabled: false,
            getAttribute: name => name === 'data-pane-id' ? String(id) : null,
            querySelector(selector) {
                if (!this.activeMounted) return null;
                if (selector === '[data-active="true"][data-view-id]') return this.active;
                if (selector.startsWith('[data-active="true"][data-view-id=')) {
                    return selector.includes(`data-view-id="${this.view}"`) ? this.active : null;
                }
                return this.active.querySelector(selector);
            },
        };
        return pane;
    });
    const setView = (id, view) => {
        const pane = panes[id - 1];
        pane.view = view;
        pane.active = {
            getAttribute: name => name === 'data-view-id' ? String(view) : null,
            querySelector: () => pane.inputMounted && !pane.disabled ? pane.textarea : null,
        };
        pane.textarea = {
            focus: options => {
                focusCalls.push({ pane: id, view, options });
                handlers.focusin?.();
            },
        };
    };
    panes.forEach(pane => setView(pane.id, pane.id * 11));
    class Element {
        constructor(pane, interactive = false) { this.pane = pane; this.interactive = interactive; }
        closest(selector) { return selector === '.pane' ? this.pane : this.interactive ? this : null; }
    }
    const document = {
        documentElement: {},
        getElementById: id => id === 'psa-shell' && shell.isConnected ? shell : null,
        querySelector(selector) {
            if (selector.includes('aria-modal')) return modal ? {} : null;
            const pane = panes[current - 1];
            if (!shell.isConnected || !pane?.connected) return null;
            if (selector === '.pane.focused') return pane;
            const id = Number(selector.match(/data-pane-id="(\d+)"/)?.[1]);
            return id === current ? pane : null;
        },
        addEventListener: (name, fn) => { handlers[name] = fn; },
        removeEventListener: (name, fn) => { assert.equal(handlers[name], fn); delete handlers[name]; },
    };
    const window = {
        getSelection: () => ({ isCollapsed: selection }),
        addEventListener: (name, fn) => { windowHandlers[name] = fn; },
        removeEventListener: (name, fn) => { assert.equal(windowHandlers[name], fn); delete windowHandlers[name]; },
    };
    const context = vm.createContext({
        document, window, Element,
        MutationObserver: class {
            constructor(callback) { this.callback = callback; }
            observe() { observers.add(this); }
            disconnect() { observers.delete(this); }
        },
        requestAnimationFrame: fn => { const id = nextId++; frames.set(id, fn); return id; },
        cancelAnimationFrame: id => frames.delete(id),
        setTimeout: fn => { const id = nextId++; timers.set(id, fn); return id; },
        clearTimeout: id => timers.delete(id),
    });
    vm.runInContext(source, context);
    return {
        window, panes, handlers, windowHandlers, shell, focusCalls, setView,
        install: () => vm.runInContext(source, context),
        click: (pane, interactive = false) => handlers.click({ button: 0, target: new Element(pane, interactive) }),
        flush: () => { while (frames.size) { const [id, fn] = frames.entries().next().value; frames.delete(id); fn(); } },
        mutate: (change = () => {}) => { change(); [...observers].forEach(observer => observer.callback([])); },
        expire: () => { [...timers.values()].forEach(fn => fn()); },
        setCurrent: id => { current = id; },
        setModal: value => { modal = value; },
        setSelection: value => { selection = value; },
        focused: () => focusCalls.at(-1)?.pane ?? null,
        resources: () => ({ observers: observers.size, timers: timers.size, frames: frames.size }),
    };
}

const cleaned = f => assert.deepEqual(f.resources(), { observers: 0, timers: 0, frames: 0 });

test('background click focuses only that pane; selection and controls are left alone', () => {
    const f = fixture();
    f.click(f.panes[1]);
    assert.equal(f.focused(), 2);
    f.click(f.panes[0], true);
    assert.equal(f.focused(), 2);
    f.setSelection(false);
    f.click(f.panes[0]);
    assert.equal(f.focused(), 2);
    f.setSelection(true);
    f.setModal(true);
    f.click(f.panes[0]);
    assert.equal(f.focused(), 2);
    assert.equal(f.focusCalls[0].options.preventScroll, true);
    cleaned(f);
});

test('existing tab title and keyboard navigation focus their newly active composer', () => {
    const f = fixture();
    f.handlers.pointerdown();
    f.click(f.panes[1], true); // Rust requests focus after the title click.
    assert.equal(f.focused(), null);
    f.window.psaFocusComposer(2, 22);
    f.setCurrent(2);
    f.flush();
    assert.equal(f.focused(), 2);
    f.window.psaFocusComposer(1, 11);
    f.setCurrent(1);
    f.flush();
    assert.equal(f.focused(), 1);
    f.click(f.panes[1], true); // Close/other controls do not request focus.
    assert.equal(f.focused(), 1);
    cleaned(f);
});

test('new tab focus retries when its active view and input mount after the first frame', () => {
    const f = fixture();
    f.window.psaFocusComposer(1, 33);
    f.flush(); // The old tab is still rendered.
    assert.equal(f.focused(), null);
    f.mutate(() => { f.setView(1, 33); f.panes[0].inputMounted = false; });
    assert.equal(f.focused(), null);
    f.mutate(() => { f.panes[0].inputMounted = true; });
    assert.equal(f.focusCalls.at(-1).view, 33);
    cleaned(f);
});

test('async preview replacement waits for draft loading, then focuses only once', () => {
    const f = fixture();
    f.window.psaBeginComposerOpen(7);
    f.mutate(); // Network/background renders do not focus anything.
    f.window.psaFocusComposer(1, 33, 7);
    f.mutate(() => { f.setView(1, 33); f.panes[0].disabled = true; });
    f.flush();
    assert.equal(f.focused(), null);
    assert.equal(f.resources().observers, 1);
    f.mutate(() => { f.panes[0].disabled = false; });
    assert.equal(f.focusCalls.at(-1).view, 33);
    cleaned(f);
    for (let i = 0; i < 10; i++) f.mutate(); // Live transcript renders never refocus.
    f.flush();
    assert.equal(f.focusCalls.length, 1);
});

test('an async open DOM commit may precede its focus effect without losing intent', () => {
    const f = fixture();
    f.window.psaBeginComposerOpen(7);
    f.mutate(() => { f.setView(1, 33); f.panes[0].disabled = true; });
    f.window.psaFocusComposer(1, 33, 7);
    f.flush();
    assert.equal(f.focused(), null);
    f.mutate(() => { f.panes[0].disabled = false; });
    assert.equal(f.focusCalls.at(-1).view, 33);
    cleaned(f);
});

test('a temporarily empty active pane while mounting a new view does not cancel focus', () => {
    const f = fixture();
    f.window.psaFocusComposer(1, 33);
    f.mutate(() => { f.panes[0].activeMounted = false; });
    f.flush();
    assert.equal(f.focused(), null);
    f.mutate(() => { f.setView(1, 33); f.panes[0].activeMounted = true; });
    assert.equal(f.focusCalls.at(-1).view, 33);
    cleaned(f);
});

test('async open/create intent cannot be refreshed after a later pointer or focus', () => {
    for (const event of ['pointerdown', 'focusin']) {
        const f = fixture();
        f.window.psaBeginComposerOpen(7); // Before create_session/list_agents await.
        f.handlers[event]();
        f.window.psaFocusComposer(1, 33, 7); // Response arrives after interaction.
        f.mutate(() => f.setView(1, 33));
        f.flush();
        assert.equal(f.focused(), null, event);
        cleaned(f);
    }
});

test('a stale async token cannot replace or cancel a newer open request', () => {
    const f = fixture();
    f.window.psaBeginComposerOpen(7);
    f.window.psaBeginComposerOpen(8);
    f.window.psaFocusComposer(1, 33, 7);
    f.window.psaFocusComposer(1, 44, 8);
    f.mutate(() => f.setView(1, 44));
    f.flush();
    assert.equal(f.focusCalls.at(-1).view, 44);
    cleaned(f);
});

test('pointer, focus and window blur cancel disabled-input retries and release resources', () => {
    for (const event of ['pointerdown', 'focusin', 'blur']) {
        const f = fixture();
        f.panes[0].disabled = true;
        f.window.psaFocusComposer(1, 11);
        f.flush();
        (event === 'blur' ? f.windowHandlers : f.handlers)[event]();
        cleaned(f);
        f.mutate(() => { f.panes[0].disabled = false; });
        assert.equal(f.focused(), null, event);
    }
});

test('a modal cancels both the retry and in-flight open intent, without resuming on close', () => {
    for (const waitingForResponse of [false, true]) {
        const f = fixture();
        f.window.psaBeginComposerOpen(7);
        if (!waitingForResponse) {
            f.panes[0].disabled = true;
            f.window.psaFocusComposer(1, 11, 7);
            f.flush();
        }
        f.mutate(() => f.setModal(true));
        cleaned(f);
        f.mutate(() => { f.setModal(false); f.panes[0].disabled = false; });
        if (waitingForResponse) f.window.psaFocusComposer(1, 11, 7);
        f.flush();
        assert.equal(f.focused(), null);
        cleaned(f);
    }
    const f = fixture();
    f.setModal(true);
    f.window.psaFocusComposer(1, 11);
    f.flush();
    assert.equal(f.focused(), null);
    cleaned(f);
});

test('a view change while an async open is pending invalidates its intent', () => {
    const f = fixture();
    f.window.psaBeginComposerOpen(7);
    f.mutate(() => f.setCurrent(2));
    f.window.psaFocusComposer(2, 33, 7);
    f.mutate(() => f.setView(2, 33));
    f.flush();
    assert.equal(f.focused(), null);
    cleaned(f);
});

test('leaving or unmounting a loading view permanently cancels focus', () => {
    for (const change of [
        f => f.setCurrent(2),
        f => f.setView(1, 99),
        f => { f.panes[0].activeMounted = false; },
        f => { f.shell.isConnected = false; },
    ]) {
        const f = fixture();
        f.panes[0].disabled = true;
        f.window.psaFocusComposer(1, 11);
        f.flush();
        f.mutate(() => change(f));
        cleaned(f);
        f.mutate(() => {
            f.shell.isConnected = true;
            f.setCurrent(1);
            f.setView(1, 11);
            f.panes[0].activeMounted = true;
            f.panes[0].disabled = false;
        });
        assert.equal(f.focused(), null);
    }
});

test('a third view supersedes a requested tab even before its first render', () => {
    const f = fixture();
    f.window.psaFocusComposer(1, 33);
    f.mutate(() => f.setView(1, 44));
    f.mutate(() => f.setView(1, 33));
    f.flush();
    assert.equal(f.focused(), null);
    cleaned(f);
});

test('missing or disabled composers have a bounded wait, including before async response', () => {
    for (const phase of ['missing', 'disabled', 'network']) {
        const f = fixture();
        if (phase === 'network') {
            f.window.psaBeginComposerOpen(7);
        } else {
            f.panes[0].disabled = phase === 'disabled';
            f.window.psaFocusComposer(1, phase === 'missing' ? 33 : 11);
            f.flush();
        }
        f.expire();
        cleaned(f);
        if (phase === 'network') f.window.psaFocusComposer(1, 33, 7);
        f.mutate(() => { f.setView(1, 33); f.panes[0].disabled = false; });
        assert.equal(f.focused(), null, phase);
        cleaned(f);
    }
});

test('installer is idempotent and removal cancels frames, observers, timers and handlers', () => {
    const f = fixture();
    const focus = f.window.psaFocusComposer;
    f.install();
    assert.equal(f.window.psaFocusComposer, focus);
    f.window.psaFocusComposer(1, 11);
    f.window.psaRemovePaneFocus();
    f.flush();
    assert.equal(f.focused(), null);
    cleaned(f);
    assert.deepEqual(Object.keys(f.handlers), []);
    assert.deepEqual(Object.keys(f.windowHandlers), []);
    assert.equal(f.window.psaFocusComposer, undefined);
    assert.equal(f.window.psaBeginComposerOpen, undefined);
    assert.equal(f.window.psaRemovePaneFocus, undefined);
});

test('background renders do not install any focus retry without an intentional request', () => {
    const f = fixture();
    for (let i = 0; i < 10; i++) f.mutate();
    f.flush();
    assert.equal(f.focused(), null);
    cleaned(f);
});
