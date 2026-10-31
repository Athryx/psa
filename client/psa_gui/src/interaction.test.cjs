const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function fixture(kind = 'divider', axis = 'horizontal') {
    class Target {
        constructor() { this.listeners = new Map(); }
        addEventListener(name, callback) {
            if (!this.listeners.has(name)) this.listeners.set(name, new Set());
            this.listeners.get(name).add(callback);
        }
        removeEventListener(name, callback) { this.listeners.get(name)?.delete(callback); }
        dispatch(name, details = {}) {
            const event = {
                type: name, target: this, pointerId: 1, button: 0, isPrimary: true,
                altKey: false, ctrlKey: false, metaKey: false, shiftKey: false,
                prevented: false, stopped: false,
                preventDefault() { this.prevented = true; },
                stopImmediatePropagation() { this.stopped = true; },
                ...details,
            };
            for (const callback of [...(this.listeners.get(name) || [])]) {
                callback(event);
                if (event.stopped) break;
            }
            return event;
        }
        listenerCount() { return [...this.listeners.values()].reduce((sum, set) => sum + set.size, 0); }
    }
    class Style {
        constructor() { this.values = new Map(); }
        setProperty(name, value, priority = '') { this.values.set(name, [value, priority]); }
        removeProperty(name) { this.values.delete(name); }
        getPropertyValue(name) { return this.values.get(name)?.[0] || ''; }
        getPropertyPriority(name) { return this.values.get(name)?.[1] || ''; }
    }
    class Element extends Target {
        constructor(id, className) {
            super();
            this.id = id;
            this.classList = { contains: name => className.split(' ').includes(name) };
            this.style = new Style();
            this.attributes = {};
            this.children = [];
            this.parentElement = null;
            this.isConnected = true;
            this.captured = new Set();
            this.captures = 0;
            this.releases = 0;
            this.value = kind === 'range' ? '14' : '';
            this.rect = { left: 0, top: 0, width: 1005, height: 805 };
        }
        closest(selector) { return selector === '.shell' ? shell : null; }
        setAttribute(name, value) { this.attributes[name] = value; }
        getBoundingClientRect() { return this.rect; }
        focus() { this.focused = true; }
        setPointerCapture(id) { this.captures++; this.captured.add(id); }
        hasPointerCapture(id) { return this.captured.has(id); }
        releasePointerCapture(id) {
            this.releases++;
            this.captured.delete(id);
            this.dispatch('lostpointercapture', { pointerId: id });
        }
    }
    const window = new Target();
    const shell = new Element('shell', 'shell');
    const split = new Element('split', 'split');
    const first = new Element('first', 'branch');
    const element = new Element('control', kind === 'divider' ? 'divider' : '');
    element.rect = { width: 5, height: 5 };
    const second = new Element('second', 'branch');
    split.children = [first, element, second];
    for (const child of split.children) child.parentElement = split;
    const nodes = new Map([shell, split, first, element, second].map(node => [node.id, node]));
    let observer = null;
    class MutationObserver {
        constructor(callback) { this.callback = callback; observer = this; }
        observe() { this.active = true; }
        disconnect() { this.active = false; }
    }
    const timers = new Map();
    let nextTimer = 0;
    window.setTimeout = callback => { timers.set(++nextTimer, callback); return nextTimer; };
    window.clearTimeout = id => timers.delete(id);
    const document = { getElementById: id => nodes.get(id), documentElement: shell };
    const source = fs.readFileSync(path.join(__dirname, 'interaction.js'), 'utf8');
    const context = vm.createContext({ window, document, MutationObserver });
    vm.runInContext(source, context);
    const commits = [];
    let disposed = 0;
    const options = kind === 'range'
        ? { id: 'control', value: 14, min: 11, max: 22, preview_property: '--psa-transcript-font-size', preview_unit: 'px' }
        : { id: 'control', split_id: 'split', path: '01', axis, value: 0.5, min: 0.1, max: 0.9 };
    const api = window.psaInteractions;
    const install = () => api[kind]('token', options, message => commits.push({ ...message }), () => disposed++);
    install();
    return {
        api, window, shell, split, first, second, element, commits, options,
        update: (changes, revision = 0) => api.update('token', { ...options, ...changes }, revision),
        input(value) { element.value = String(value); return element.dispatch('input'); },
        flush() {
            const pending = [...timers.values()];
            timers.clear();
            for (const callback of pending) callback();
        },
        start: (details = {}) => element.dispatch('pointerdown', { clientX: 500, clientY: 400, ...details }),
        move: (details = {}) => window.dispatch('pointermove', { clientX: 600, clientY: 480, ...details }),
        end: (name = 'pointerup', details = {}) => window.dispatch(name, details),
        detach() { element.isConnected = false; observer.callback(); },
        remove: () => api.remove('token'),
        disposed: () => disposed,
        observing: () => observer.active,
        timerCount: () => timers.size,
        install,
        reload: () => vm.runInContext(source, context),
    };
}

function approx(actual, expected) {
    assert.ok(Math.abs(actual - expected) < 1e-10, `${actual} != ${expected}`);
}

function ratio(f) { return Number(f.first.style.flex.split(' ')[0]); }

test('range previews natively for an entire drag and publishes only its final value', () => {
    const f = fixture('range');
    const down = f.start();
    assert.equal(down.prevented, false, 'the native slider retains its default action');
    assert.equal(f.element.captures, 1);
    for (let value = 15; value <= 21; value++) {
        assert.equal(f.input(value).stopped, true, 'no delegated renderer input');
        assert.equal(f.shell.style.getPropertyValue('--psa-transcript-font-size'), `${value}px`);
        f.flush();
        assert.deepEqual(f.commits, []);
    }
    f.end();
    assert.deepEqual(f.commits, [{ value: 21, revision: 1 }]);
    f.element.dispatch('change');
    assert.equal(f.commits.length, 1);
    assert.equal(f.element.releases, 1);
    assert.equal(f.window.listenerCount(), 0);
});

test('range keyboard and assistive input commits are coalesced with no controlled-value rewind', () => {
    const f = fixture('range');
    const key = f.element.dispatch('keydown', { key: 'ArrowRight' });
    assert.equal(key.stopped, true);
    assert.equal(key.prevented, false);
    f.input(15);
    f.element.dispatch('change'); // native keyboard change, not a pointer release
    f.input(16);
    f.element.dispatch('change');
    f.update({ value: 14 }); // unrelated render while the native thumb is ahead
    assert.equal(f.element.value, '16');
    assert.deepEqual(f.commits, []);
    f.element.dispatch('keyup', { key: 'ArrowRight' });
    assert.deepEqual(f.commits, [{ value: 16, revision: 1 }]);
    f.input(17);
    f.update({ value: 16 }, 1); // earlier semantic echo while a newer key is held
    assert.equal(f.element.value, '17');
    f.flush();
    assert.deepEqual(f.commits, [{ value: 16, revision: 1 }, { value: 17, revision: 2 }]);
    f.update({ value: 16 }, 1); // delayed echo after the next local commit
    assert.equal(f.element.value, '17');
    f.update({ value: 17 }, 2);
    assert.equal(f.shell.style.getPropertyValue('--psa-transcript-font-size'), '');
    f.update({ value: 12 }, 2); // an explicit external setting change, not a stale echo
    assert.equal(f.element.value, '12');
});

test('assistive range input/change bursts without pointer or key events are coalesced', () => {
    const f = fixture('range');
    for (const value of [15, 16, 17, 18]) {
        f.input(value);
        assert.equal(f.element.dispatch('change').stopped, true);
    }
    assert.deepEqual(f.commits, []);
    f.flush();
    assert.deepEqual(f.commits, [{ value: 18, revision: 1 }]);
});

test('workspace modifier shortcuts and non-movement keys are not intercepted', () => {
    for (const kind of ['range', 'divider']) {
        const f = fixture(kind);
        for (const details of [
            { key: 'End', ctrlKey: true },
            { key: 'ArrowRight', altKey: true },
            { key: 'w', ctrlKey: true },
            { key: 'Escape' },
            { key: 'Tab' },
        ]) {
            const event = f.element.dispatch('keydown', details);
            assert.equal(event.stopped, false, `${kind}: ${details.key}`);
            assert.equal(event.prevented, false, `${kind}: ${details.key}`);
        }
        assert.deepEqual(f.commits, []);
    }
});

test('range ignores element blur during native focus transfer but commits on window blur', () => {
    const f = fixture('range');
    f.start();
    f.window.dispatch('blur', { target: {} });
    f.input(18);
    assert.equal(f.window.listenerCount(), 3);
    f.window.dispatch('blur');
    assert.deepEqual(f.commits, [{ value: 18, revision: 1 }]);
    assert.equal(f.window.listenerCount(), 0);
    assert.equal(f.element.captured.size, 0);
});

test('divider moves immediately outside its split, ignores other pointers, and commits once on window release', () => {
    const f = fixture();
    assert.equal(f.start().prevented, true);
    assert.equal(f.element.focused, true);
    f.move({ pointerId: 9, clientX: 900 });
    assert.equal(ratio(f), 0.5);
    for (let clientX = 510; clientX <= 700; clientX += 10) {
        f.move({ clientX });
        f.flush();
    }
    approx(ratio(f), 0.7);
    assert.equal(f.element.attributes['aria-valuenow'], '70');
    assert.deepEqual(f.commits, []);
    f.end('pointerup', { pointerId: 9 });
    assert.equal(f.window.listenerCount(), 4);
    f.end('pointerup', { clientX: 800 }); // final release coordinate need not have a move
    approx(ratio(f), 0.8);
    assert.equal(f.commits.length, 1);
    approx(f.commits[0].value, 0.8);
    assert.equal(f.element.releases, 1);
    assert.equal(f.window.listenerCount(), 0);
    f.move({ clientX: 900 });
    approx(ratio(f), 0.8);
});

test('unrelated renders and delayed commit patches cannot overwrite an ongoing divider drag', () => {
    const f = fixture();
    f.start();
    f.move({ clientX: 600 });
    f.first.style.flex = '0.5'; // simulate Dioxus applying a semantic branch style
    f.second.style.flex = '0.5';
    f.update({ value: 0.5 });
    approx(ratio(f), 0.6);
    f.end();
    f.start();
    f.move({ clientX: 650 });
    f.first.style.flex = '0.6';
    f.update({ value: 0.6 }, 1);
    approx(ratio(f), 0.75);
    f.end();
    f.first.style.flex = '0.6';
    f.update({ value: 0.6 }, 1);
    approx(ratio(f), 0.75);
    f.update({ value: 0.75 }, 2);
    approx(ratio(f), 0.75);
    assert.equal(f.commits.length, 2);
});

test('divider cancel, lost capture, and window blur clean up and commit the last visible ratio', () => {
    for (const ending of ['pointercancel', 'lostpointercapture', 'blur']) {
        const f = fixture();
        f.start();
        f.move();
        if (ending === 'lostpointercapture') f.element.dispatch(ending);
        else f.end(ending);
        assert.equal(f.commits.length, 1, ending);
        approx(f.commits[0].value, 0.6);
        assert.equal(f.window.listenerCount(), 0, ending);
        assert.equal(f.element.captured.size, 0, ending);
        f.remove();
        assert.equal(f.disposed(), 1, ending);
    }
});

test('vertical divider keyboard uses live ratio, axis-specific arrows, Home/End and coalesced repeat', () => {
    const f = fixture('divider', 'vertical');
    assert.equal(f.element.dispatch('keydown', { key: 'ArrowRight' }).prevented, false);
    for (let i = 0; i < 4; i++) f.element.dispatch('keydown', { key: 'ArrowDown' });
    approx(ratio(f), 0.58);
    assert.deepEqual(f.commits, []);
    f.element.dispatch('keyup', { key: 'ArrowDown' });
    approx(f.commits[0].value, 0.58);
    f.element.dispatch('keydown', { key: 'ArrowUp', shiftKey: true });
    f.flush();
    approx(ratio(f), 0.48);
    f.element.dispatch('keydown', { key: 'Home' });
    f.element.dispatch('keyup', { key: 'Home' });
    assert.equal(ratio(f), 0.1);
    f.element.dispatch('keydown', { key: 'End' });
    f.element.dispatch('keyup', { key: 'End' });
    assert.equal(ratio(f), 0.9);
    assert.equal(f.element.attributes['aria-valuenow'], '90');
    f.start();
    f.move({ clientY: 240 });
    approx(ratio(f), 0.7);
    f.end();
});

test('capture failure still releases through window listeners; invalid geometry never starts', () => {
    const f = fixture();
    f.element.setPointerCapture = () => { throw Error('unsupported'); };
    f.start();
    f.move({ clientX: 2000 });
    assert.equal(ratio(f), 0.9);
    f.end();
    assert.equal(f.commits.length, 1);
    assert.equal(f.window.listenerCount(), 0);
    f.split.rect.width = 5;
    f.start();
    assert.equal(f.window.listenerCount(), 0);
    f.split.rect.width = 1005;
    f.start({ button: 2 });
    f.start({ isPrimary: false });
    assert.equal(f.window.listenerCount(), 0);
});

test('unmount/removal cancels pending work, removes listeners/capture, and restores shell preview', () => {
    for (const detach of [false, true]) {
        const f = fixture('range');
        f.shell.style.setProperty('--psa-transcript-font-size', '13px', 'important');
        f.start();
        f.input(19);
        if (detach) f.detach();
        else f.remove();
        f.flush();
        assert.deepEqual(f.commits, []);
        assert.equal(f.element.listenerCount(), 0);
        assert.equal(f.window.listenerCount(), 0);
        assert.equal(f.element.captured.size, 0);
        assert.equal(f.disposed(), 1);
        assert.equal(f.observing(), false);
        assert.equal(f.shell.style.getPropertyValue('--psa-transcript-font-size'), '13px');
        assert.equal(f.shell.style.getPropertyPriority('--psa-transcript-font-size'), 'important');
        f.remove();
        assert.equal(f.disposed(), 1);
    }
    const f = fixture();
    f.element.dispatch('keydown', { key: 'ArrowRight' });
    assert.equal(f.timerCount(), 1);
    f.detach();
    f.flush();
    assert.deepEqual(f.commits, []);
});

test('reinstall/repeated script evaluation does not duplicate listeners or commits', () => {
    const f = fixture();
    const count = f.element.listenerCount();
    f.reload();
    f.install();
    assert.equal(f.disposed(), 1);
    assert.equal(f.element.listenerCount(), count);
    f.start();
    f.move();
    f.end();
    assert.equal(f.commits.length, 1);
    f.remove();
    assert.equal(f.element.listenerCount(), 0);
});
