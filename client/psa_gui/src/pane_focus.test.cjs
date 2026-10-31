const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture() {
    const handlers = {};
    const frames = [];
    let modal = false, selection = true, focused = null;
    const panes = [1, 2].map(id => {
        const textarea = { focus: () => { focused = id; } };
        const active = { querySelector: () => textarea };
        return { id, querySelector: selector => selector.startsWith('[data-active="true"][data-view-id=')
            ? (selector.includes(`data-view-id="${id * 11}"`) ? active : null)
            : textarea }; 
    });
    let current = 1;
    class Element {
        constructor(pane, interactive = false) { this.pane = pane; this.interactive = interactive; }
        closest(selector) { return selector === '.pane' ? this.pane : this.interactive ? this : null; }
    }
    const document = {
        querySelector(selector) {
            if (selector.includes('aria-modal')) return modal ? {} : null;
            const id = Number(selector.match(/data-pane-id="(\d+)"/)?.[1]);
            return id === current ? panes[id - 1] : null;
        },
        addEventListener: (name, fn) => { handlers[name] = fn; },
        removeEventListener: name => { delete handlers[name]; },
    };
    const window = { getSelection: () => ({ isCollapsed: selection }) };
    vm.runInNewContext(fs.readFileSync(path.join(__dirname, 'pane_focus.js'), 'utf8'), {
        document, window, Element, requestAnimationFrame: fn => frames.push(fn),
    });
    return {
        window, panes, handlers,
        click: (pane, interactive = false) => handlers.click({ button: 0, target: new Element(pane, interactive) }),
        flush: () => { while (frames.length) frames.shift()(); },
        setCurrent: id => { current = id; },
        setModal: value => { modal = value; },
        setSelection: value => { selection = value; },
        focused: () => focused,
    };
}

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
});

test('a tab selection focuses its newly rendered composer without redirecting control clicks', () => {
    const f = fixture();
    f.handlers.pointerdown();
    f.click(f.panes[1], true); // Tab title is interactive; Rust requests focus after selecting it.
    assert.equal(f.focused(), null);
    f.window.psaFocusComposer(2, 22);
    f.setCurrent(2);
    f.flush();
    assert.equal(f.focused(), 2);
    f.click(f.panes[0], true); // Close and other controls do not request focus.
    assert.equal(f.focused(), 2);
});

test('keyboard selection waits for rendering and stale requests never steal focus', () => {
    const f = fixture();
    f.window.psaFocusComposer(2, 22);
    f.setCurrent(2);
    f.flush();
    assert.equal(f.focused(), 2);
    f.window.psaFocusComposer(1, 11);
    f.handlers.pointerdown();
    f.setCurrent(1);
    f.flush();
    assert.equal(f.focused(), 2);
    f.window.psaFocusComposer(1, 11);
    f.setModal(true);
    f.flush();
    assert.equal(f.focused(), 2);
    f.setModal(false);
    f.window.psaFocusComposer(1, 99); // the requested tab was replaced before render
    f.flush();
    assert.equal(f.focused(), 2);
    f.window.psaFocusComposer(1, 11);
    f.handlers.focusin(); // another control was focused before the frame
    f.flush();
    assert.equal(f.focused(), 2);
    f.window.psaFocusComposer(1, 11);
    f.window.psaRemovePaneFocus();
    f.flush();
    assert.equal(f.focused(), 2);
});
