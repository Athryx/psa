const { test } = require('node:test');
const assert = require('node:assert/strict');
const { createHoveredWheelRouter } = require('./wheel.js');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function fixture({ install = true } = {}) {
    const handlers = new Set(), keyHandlers = new Set(), clickHandlers = new Set(), trace = [];
    const listeners = { wheel: handlers, keydown: keyHandlers, click: clickHandlers };
    let hit = null;
    class Element {
        constructor(classes = '', parent = null, options = {}) {
            this.nodeType = 1;
            this.classes = classes.split(' ');
            this.parentElement = parent;
            this.children = [];
            parent?.children.push(this);
            this.dataset = {};
            this.computed = { overflowX: 'visible', overflowY: 'visible', lineHeight: '20px', fontSize: '14px', direction: 'ltr', ...options.computed };
            this.clientHeight = options.clientHeight ?? 500;
            this.scrollHeight = options.scrollHeight ?? 2000;
            this.clientWidth = options.clientWidth ?? 500;
            this.scrollWidth = options.scrollWidth ?? 500;
            this.scrollTop = options.scrollTop ?? 100;
            this.scrollLeft = options.scrollLeft ?? 0;
            this.events = [];
        }
        matches(selector) {
            if (selector === '[aria-modal="true"]' || selector === 'dialog[open]') return !!this.modal;
            return selector.startsWith('.') && this.classes.includes(selector.slice(1));
        }
        closest(selector) {
            for (let current = this; current; current = current.parentElement) {
                if (selector.split(',').some(part => current.matches(part.trim()))) return current;
            }
            return null;
        }
        querySelector(selector) {
            const all = [];
            const visit = current => { for (const child of current.children) { all.push(child); visit(child); } };
            visit(this);
            if (selector === '[data-active="true"] .transcript') return all.find(node => node.matches('.transcript') && node.parentElement.dataset.active === 'true') ?? null;
            return all.find(node => node.matches(selector)) ?? null;
        }
        dispatchEvent(event) {
            this.events.push(event);
            trace.push({ node: this, type: event.type, top: this.scrollTop, detail: event.detail });
            this.onintent?.(event);
            if (event.type === 'psa:transcript-follow') this.onfollow?.(event);
            return !event.defaultPrevented;
        }
    }
    const document = {
        activeElement: null,
        elementFromPoint: () => hit,
        querySelector(selector) {
            if (selector.includes('aria-modal')) return document.modal ?? null;
            if (selector === '.pane.focused') return panes[0].pane;
            return null;
        },
        addEventListener(name, handler, options) {
            if (name === 'wheel') {
                assert.equal(options.capture, true);
                assert.equal(options.passive, false);
            } else assert.equal(options, true);
            listeners[name].add(handler);
        },
        removeEventListener(name, handler, capture) {
            assert.equal(capture, true);
            listeners[name].delete(handler);
        },
    };
    const window = {
        getComputedStyle: node => node.computed,
        CustomEvent: class {
            constructor(type, { detail, cancelable = false }) { this.type = type; this.detail = detail; this.cancelable = cancelable; this.defaultPrevented = false; }
            preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
        },
    };
    const shell = new Element('shell');
    const panes = [1, 2].map(() => {
        const pane = new Element('pane', shell);
        const view = new Element('', pane); view.dataset.active = 'true';
        const transcript = new Element('transcript', view, { computed: { overflowY: 'auto', overflowX: 'auto' } });
        const composer = new Element('composer', view);
        const textarea = new Element('', composer, { clientHeight: 80, scrollHeight: 80, scrollTop: 0, computed: { overflowY: 'auto' } });
        textarea.selectionStart = 7; textarea.selectionEnd = 11;
        return { pane, view, transcript, composer, textarea };
    });
    document.activeElement = panes[0].textarea;
    hit = panes[0].transcript;
    let router = install ? createHoveredWheelRouter(document, window) : null;
    return {
        Element, shell, panes, document, window, trace, handlers, keyHandlers, clickHandlers,
        key(options = {}) {
            const event = { target: panes[0].textarea, key:'End', ctrlKey:true, defaultPrevented:false,
                preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.stopped = true; }, ...options };
            for (const handler of keyHandlers) handler(event);
            return event;
        },
        click(target) {
            const event = { target, button:0, defaultPrevented:false,
                preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.stopped = true; } };
            for (const handler of clickHandlers) handler(event);
            return event;
        },
        hover: node => { hit = node; },
        wheel(options = {}) {
            const event = { target: panes[0].textarea, clientX: 50, clientY: 50, deltaMode: 0, deltaX: 0, deltaY: 40, cancelable: true, defaultPrevented: false,
                preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.stopped = true; }, ...options };
            for (const handler of handlers) handler(event);
            return event;
        },
        dispose: () => router?.dispose(),
        reinstall: () => { router = createHoveredWheelRouter(document, window); },
    };
}

test('wheel uses the hovered split even while a different prompt owns keyboard focus', () => {
    const f = fixture(); f.hover(f.panes[1].transcript);
    const event = f.wheel();
    assert.equal(f.panes[1].transcript.scrollTop, 140);
    assert.equal(f.panes[0].transcript.scrollTop, 100);
    assert.equal(f.document.activeElement, f.panes[0].textarea);
    assert.deepEqual([f.document.activeElement.selectionStart, f.document.activeElement.selectionEnd], [7, 11]);
    assert.equal(event.defaultPrevented, true);
    assert.equal(event.stopped, true);
});

test('a short prompt routes the wheel to its own transcript without changing focus or caret', () => {
    const f = fixture(); f.hover(f.panes[0].textarea);
    f.wheel({ deltaY: -30 });
    assert.equal(f.panes[0].transcript.scrollTop, 70);
    assert.equal(f.panes[0].textarea.scrollTop, 0);
    assert.equal(f.document.activeElement, f.panes[0].textarea);
});

test('a long prompt or nested tool scrolls first, then chains to the same hovered transcript at its edge', () => {
    const f = fixture(); const p = f.panes[1];
    p.textarea.scrollHeight = 200; f.hover(p.textarea);
    f.wheel({ deltaY: 40 });
    assert.equal(p.textarea.scrollTop, 40); assert.equal(p.transcript.scrollTop, 100);
    p.textarea.scrollTop = 120;
    f.wheel({ deltaY: 40 });
    assert.equal(p.transcript.scrollTop, 140);
    const tool = new f.Element('', p.transcript, { clientHeight: 200, scrollHeight: 600, scrollTop: 10, computed: { overflowY: 'auto' } });
    f.hover(tool); f.wheel({ deltaY: -10 });
    assert.equal(tool.scrollTop, 0); assert.equal(p.transcript.scrollTop, 140);
    f.wheel({ deltaY: -20 });
    assert.equal(p.transcript.scrollTop, 120);
    assert.equal(f.panes[0].transcript.scrollTop, 100);
});

test('wheel units normalize lines/pages while precise trackpad pixels retain their magnitude', () => {
    const f = fixture(); const root = f.panes[0].transcript;
    f.wheel({ deltaY: 3, deltaMode: 1 }); assert.equal(root.scrollTop, 172);
    f.wheel({ deltaY: 1, deltaMode: 2 }); assert.equal(root.scrollTop, 672);
    f.wheel({ deltaY: 0.25 }); assert.equal(root.scrollTop, 672.25);
    f.wheel({ deltaY: 4, wheelDeltaY: -12 }); assert.equal(root.scrollTop, 676.25);
});

test('tiny physical-wheel detents are raised to useful steps without reducing larger wheel deltas', () => {
    const f = fixture(); const root = f.panes[0].transcript;
    f.wheel({ deltaY: 1, wheelDeltaY: -120 }); assert.equal(root.scrollTop, 172);
    f.wheel({ deltaY: -1, wheelDeltaY: 120 }); assert.equal(root.scrollTop, 100);
    f.wheel({ deltaY: 2, wheelDeltaY: -240 }); assert.equal(root.scrollTop, 244);
    f.wheel({ deltaY: 100, wheelDeltaY: -120 }); assert.equal(root.scrollTop, 344);
});

test('intent reaches the transcript before moving, including at a history boundary', () => {
    const f = fixture(); const root = f.panes[0].transcript;
    let following = true;
    root.onintent = event => { if (event.detail.deltaY < 0) following = false; };
    root.scrollTop = 1490;
    f.wheel({ deltaY: -1 });
    assert.equal(following, false);
    assert.equal(f.trace[0].type, 'psa:transcript-user-scroll');
    assert.equal(f.trace[0].top, 1490);
    assert.equal(root.scrollTop, 1489);
    root.scrollTop = 0;
    f.wheel({ deltaY: -10 });
    assert.equal(root.scrollTop, 0);
    assert.equal(f.trace.length, 2);
});

test('sidebar and settings scroll under the mouse, never the focused background pane', () => {
    const f = fixture();
    const sidebar = new f.Element('sidebar', f.shell, { computed: { overflowY: 'auto' } });
    f.hover(sidebar); f.wheel({ deltaY: 50 }); assert.equal(sidebar.scrollTop, 150);
    const backdrop = new f.Element('modal-backdrop', f.shell);
    const dialog = new f.Element('settings-dialog', backdrop); dialog.modal = true;
    const header = new f.Element('', dialog);
    const content = new f.Element('settings-content', dialog, { computed: { overflowY: 'auto' } });
    f.hover(header); f.wheel({ deltaY: 60 }); assert.equal(content.scrollTop, 160);
    f.hover(backdrop); assert.equal(f.wheel().defaultPrevented, true);
    assert.equal(f.panes[0].transcript.scrollTop, 100);
});

test('Shift-wheel scrolls nested code horizontally and a hovered tab strip uses its horizontal overflow', () => {
    const f = fixture();
    const code = new f.Element('', f.panes[0].transcript, { scrollWidth: 1400, computed: { overflowX: 'auto' } });
    f.hover(code); f.wheel({ shiftKey: true, deltaY: 40 }); assert.equal(code.scrollLeft, 40);
    assert.equal(f.panes[0].transcript.scrollTop, 100);
    const tabs = new f.Element('tabs', f.panes[1].pane, { scrollWidth: 1600, clientHeight: 40, scrollHeight: 40, computed: { overflowX: 'auto' } });
    f.hover(tabs); f.wheel({ deltaY: 60 }); assert.equal(tabs.scrollLeft, 60);
    assert.equal(f.panes[1].transcript.scrollTop, 100);
});

test('horizontal trackpad movement respects RTL scroll bounds', () => {
    const f = fixture();
    const code = new f.Element('', f.panes[0].transcript, { scrollWidth: 1400, computed: { overflowX: 'auto', direction: 'rtl' } });
    f.hover(code); f.wheel({ deltaX: -100, deltaY: 0 }); assert.equal(code.scrollLeft, -100);
});

test('zoom gestures, canceled/noncancelable events and zero deltas are left native', () => {
    const f = fixture();
    for (const options of [{ ctrlKey: true }, { metaKey: true }, { defaultPrevented: true }, { cancelable: false }, { deltaY: 0 }, { deltaY: NaN }]) {
        const event = f.wheel(options);
        assert.equal(event.stopped, undefined);
    }
    assert.equal(f.panes[0].transcript.scrollTop, 100);
    assert.equal(f.trace.length, 0);
});

test('scrolling at an exhausted sidebar does not spill into a different pane', () => {
    const f = fixture();
    const sidebar = new f.Element('sidebar', f.shell, { scrollTop: 1500, computed: { overflowY: 'auto' } });
    f.hover(sidebar); f.wheel({ deltaY: 500 });
    assert.equal(sidebar.scrollTop, 1500);
    assert.equal(f.panes[0].transcript.scrollTop, 100);
});

test('teardown removes routing and repeated script installation never doubles wheel movement', () => {
    const f = fixture(); f.dispose();
    f.wheel(); assert.equal(f.panes[0].transcript.scrollTop, 100);
    const source = fs.readFileSync(path.join(__dirname, 'wheel.js'), 'utf8');
    const environment = { document: f.document, window: f.window };
    vm.runInNewContext(source, environment); vm.runInNewContext(source, environment);
    assert.equal(f.handlers.size, 1);
    f.wheel(); assert.equal(f.panes[0].transcript.scrollTop, 140);
    f.window.psaRemoveHoveredWheel(); assert.equal(f.handlers.size, 0);
    assert.equal(f.keyHandlers.size, 0); assert.equal(f.clickHandlers.size, 0);
});

test('Ctrl-End and later wheel intent reach the same focused-pane controller in native order', () => {
    const f = fixture(); const root = f.panes[0].transcript;
    const actions = [];
    root.onfollow = event => { event.preventDefault(); actions.push('follow'); };
    root.onintent = event => { if (event.type === 'psa:transcript-user-scroll') actions.push('wheel'); };
    f.hover(f.panes[1].transcript);
    const event = f.key();
    assert.equal(event.defaultPrevented, true); assert.equal(event.stopped, true);
    assert.equal(f.panes[1].transcript.events.length, 0, 'keyboard follows focus, not the hovered pane');
    f.hover(root); f.wheel({ deltaY:-1 });
    assert.deepEqual(actions, ['follow', 'wheel']);
    assert.equal(f.document.activeElement, f.panes[0].textarea);
});

test('explicit follow falls back to Rust until a controller accepts, and never acts through a modal', () => {
    const f = fixture();
    assert.equal(f.key().defaultPrevented, false);
    f.panes[0].transcript.onfollow = event => event.preventDefault();
    for (const options of [{ shiftKey:true }, { altKey:true }, { metaKey:true }, { ctrlKey:false }, { key:'ArrowDown' }, { defaultPrevented:true }]) {
        assert.equal(f.key(options).stopped, undefined);
    }
    const count = f.panes[0].transcript.events.length;
    f.document.modal = {};
    assert.equal(f.key().defaultPrevented, false);
    assert.equal(f.panes[0].transcript.events.length, count);
});

test('Follow activity clicks use native ordering only when their own transcript accepts', () => {
    const f = fixture(); const root = f.panes[1].transcript;
    const follow = new f.Element('new-activity', root);
    assert.equal(f.click(follow).defaultPrevented, false);
    root.onfollow = event => event.preventDefault();
    const event = f.click(follow);
    assert.equal(event.defaultPrevented, true); assert.equal(event.stopped, true);
    assert.equal(f.panes[0].transcript.events.length, 0);
    assert.equal(f.click(f.panes[0].textarea).defaultPrevented, false);
});
