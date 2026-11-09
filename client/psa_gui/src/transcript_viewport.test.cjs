const { test } = require('node:test');
const assert = require('node:assert/strict');
const { createTranscriptViewport } = require('./transcript_viewport.js');
const fs = require('node:fs');
const path = require('node:path');

function fixture({ start = 100, count = 1000, height = () => 60, following = false } = {}) {
    let mounted = [], nextFrame = 0, disposedObservers = 0;
    const frames = new Map(), handlers = {}, messages = [];
    const top = { style: { height: '0px' } }, bottom = { style: { height: '0px' } };
    const observers = [];
    let heights = height, identityShift = 0, scrollTop = (start + 60) * 60;
    const root = {
        dataset: { windowStart: String(start), windowEnd: String(Math.min(start + 150, count)), rowCount: String(count), firstRow: `c:${start}`, active: 'true', following: String(following), followRevision: '0', scrollRevision: '0', hasOlder: 'false', historyBusy: 'false' },
        get scrollTop() { return scrollTop; },
        set scrollTop(value) { scrollTop = Math.max(0, Math.min(value, Math.max(0, this.scrollHeight - this.clientHeight))); },
        clientHeight: 600, clientTop: 0,
        get scrollHeight() { return parseFloat(top.style.height) + parseFloat(bottom.style.height) + mounted.reduce((sum, row) => sum + heights(row.logicalIndex), 0); },
        getBoundingClientRect: () => ({ top: 10 }),
        querySelector: selector => selector.includes('"top"') ? top : bottom,
        querySelectorAll: () => mounted,
        addEventListener: (name, callback) => { handlers[name] = callback; },
        removeEventListener: name => { delete handlers[name]; },
    };
    const render = (newStart, newCount = Number(root.dataset.rowCount), shift = identityShift) => {
        identityShift = shift;
        root.dataset.windowStart = String(newStart);
        root.dataset.windowEnd = String(Math.min(newStart + 150, newCount));
        root.dataset.rowCount = String(newCount);
        root.dataset.firstRow = `c:${newStart - shift}`;
        mounted = Array.from({ length: Math.min(150, newCount - newStart) }, (_, offset) => {
            const logicalIndex = newStart + offset - shift;
            const row = { logicalIndex, dataset: { transcriptRow: `c:${logicalIndex}`, rowIndex: String(newStart + offset) } };
            row.getBoundingClientRect = () => {
                const position = parseFloat(top.style.height) + mounted.slice(0, offset).reduce((sum, row) => sum + heights(row.logicalIndex), 0);
                const topY = 10 + position - root.scrollTop;
                const height = heights(logicalIndex) - 12;
                return { top: topY, bottom: topY + height, height };
            };
            return row;
        });
    };
    render(start, count);
    class Observer {
        constructor(callback) { this.callback = callback; this.observed = new Set(); observers.push(this); }
        observe(node) { this.observed.add(node); }
        unobserve(node) { this.observed.delete(node); }
        disconnect() { this.observed.clear(); disposedObservers++; }
    }
    const controller = createTranscriptViewport(root, message => messages.push(message), {
        requestAnimationFrame: callback => { const id = ++nextFrame; frames.set(id, callback); return id; },
        cancelAnimationFrame: id => frames.delete(id), ResizeObserver: Observer, MutationObserver: Observer,
    });
    const flush = () => {
        let iterations = 0;
        while (frames.size) {
            assert.ok(++iterations < 20, 'observers must not schedule an endless layout loop');
            const pending = Array.from(frames); frames.clear();
            for (const [, callback] of pending) callback();
        }
    };
    const notify = () => { for (const observer of observers) observer.callback(); flush(); };
    const scroll = value => { root.scrollTop = value; handlers.scroll(); flush(); };
    const wheel = (deltaY, { native = false, scrollEvent = true } = {}) => {
        if (native) handlers.wheel({ deltaY });
        else handlers['psa:transcript-user-scroll']({ detail: { deltaY } });
        root.scrollTop += deltaY;
        if (scrollEvent) handlers.scroll();
        flush();
    };
    const follow = () => {
        const event = { cancelable: true, defaultPrevented: false,
            preventDefault() { this.defaultPrevented = true; } };
        handlers['psa:transcript-follow']?.(event);
        return event.defaultPrevented;
    };
    const offset = id => mounted.find(row => row.dataset.transcriptRow === id)?.getBoundingClientRect().top - 10;
    return { root, controller, flush, notify, scroll, wheel, follow, render, offset, messages, handlers, frames,
        setHeights: value => { heights = value; }, observerDisposals: () => disposedObservers,
        mounted: () => mounted, spacerTop: () => parseFloat(top.style.height),
    };
}

test('long sequential browsing requests bounded overlapping windows, not growing tails', () => {
    const f = fixture(); f.flush();
    for (let step = 0; step < 20; step++) {
        const start = Number(f.root.dataset.windowStart);
        f.scroll(f.spacerTop() + 20 * 60);
        const request = f.messages.at(-1);
        assert.equal(request.start, Math.max(0, start + 20 - 75));
        f.render(request.start); f.notify();
        assert.equal(f.mounted().length, 150);
        if (request.start === 0) break;
    }
});

test('recycling variable-height rows preserves the stable event and pixel offset', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 20 * 60 + 17);
    const before = f.offset('c:120');
    const request = f.messages.at(-1);
    f.setHeights(index => index < 100 ? 160 : 60);
    f.render(request.start); f.notify();
    assert.equal(f.offset('c:120'), before);
    assert.equal(f.mounted().length, 150);
});

test('a shared history prepend and simultaneous live append preserve an independent pane anchor', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 60 * 60 + 23);
    const before = f.offset('c:160');
    f.render(250, 1157, 150); f.notify();
    assert.equal(f.offset('c:160'), before);
});

test('expanding a detail above the viewport and changing width keep the viewed event fixed', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 60 * 60 + 9);
    const before = f.offset('c:160');
    f.setHeights(index => index === 110 ? 2060 : 75); f.notify();
    assert.equal(f.offset('c:160'), before);
});

test('user movement during a queued replacement supersedes its old anchor', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 20 * 60);
    const request = f.messages.at(-1);
    f.scroll(f.spacerTop() + 25 * 60 + 4);
    const before = f.offset('c:125');
    f.setHeights(index => index < 100 ? 130 : 60);
    f.render(request.start); f.notify();
    assert.equal(f.offset('c:125'), before);
});

test('scrollbar jumps into an unmounted spacer are not pulled back to an obsolete row', () => {
    const f = fixture(); f.flush();
    f.scroll(40_000);
    const request = f.messages.at(-1);
    assert.ok(request.start > 400);
    f.render(request.start); f.notify();
    assert.ok(f.root.scrollTop >= 39_000, 'thumb jump destination survives row replacement');
});

test('hidden panes suspend measurement and requests, then restore their local offset on activation', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 60 * 60 + 31);
    const before = f.offset('c:160'), scroll = f.root.scrollTop, messages = f.messages.length;
    f.root.dataset.active = 'false';
    f.setHeights(() => 100); f.notify();
    assert.equal(f.root.scrollTop, scroll);
    assert.equal(f.messages.length, messages);
    f.root.dataset.active = 'true'; f.notify();
    assert.equal(f.offset('c:160'), before);
});

test('follow mode keeps the bounded tail at the bottom after streaming resize and appends', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    f.setHeights(() => 100); f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    f.render(860, 1010); f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    assert.equal(f.mounted().length, 150);
});

test('one-pixel upward wheel intent disables follow before scrolling and ignores stale Rust attributes', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.handlers['psa:transcript-user-scroll']({ detail: { deltaY: -1 } });
    assert.equal(f.messages.at(-1).kind, 'scroll');
    assert.equal(f.messages.at(-1).following, false, 'intent is synchronous, before scrollTop changes');
    assert.equal(f.root.scrollTop, end);
    f.root.scrollTop -= 1;
    f.handlers.scroll(); f.flush();
    assert.equal(f.root.scrollTop, end - 1);
    assert.equal(f.root.dataset.following, 'true', 'Rust has not acknowledged the intent');
    f.notify(); f.notify();
    assert.equal(f.root.scrollTop, end - 1, 'stale true cannot snap back on mutation/resize');
    const before = f.offset('c:989');
    f.render(850, 1002); f.notify();
    assert.equal(f.offset('c:989'), before, 'live activity preserves the browsing anchor');
    assert.equal(f.messages.filter(message => message.kind === 'scroll').length, 1);
});

test('routed wheels survive refresh before their asynchronous native scroll event', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.controller.refresh();
    f.wheel(-7, { scrollEvent: false });
    assert.equal(f.root.scrollTop, end - 7);
    f.handlers.scroll(); f.flush();
    assert.equal(f.root.scrollTop, end - 7);
});

test('streaming layout between wheel intent and scroll observation preserves the anchor plus user displacement', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const before = f.offset('c:990');
    f.handlers['psa:transcript-user-scroll']({ detail: { deltaY: -3 } });
    f.root.scrollTop -= 3;
    f.setHeights(index => index === 900 ? 2060 : 60);
    f.handlers.scroll(); f.flush();
    assert.equal(f.offset('c:990'), before + 3);
    f.notify();
    assert.equal(f.offset('c:990'), before + 3);
});

test('subpixel upward steps and native wheel fallback do not resume follow inside end tolerance', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.wheel(-0.25, { native: true });
    assert.equal(f.root.scrollTop, end - 0.25);
    assert.equal(f.messages.at(-1).following, false);
    f.notify();
    assert.equal(f.root.scrollTop, end - 0.25);
    f.wheel(-4, { native: true });
    assert.equal(f.root.scrollTop, end - 4.25);
    assert.equal(f.messages.filter(message => message.kind === 'scroll').length, 1);
});

test('native scrollbar movement leaves follow immediately and reaching the real end restores it', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.scroll(end - 2);
    assert.equal(f.root.scrollTop, end - 2);
    assert.equal(f.messages.at(-1).following, false);
    f.root.dataset.following = 'false'; f.root.dataset.scrollRevision = '1'; f.notify();
    f.scroll(end - 1);
    assert.equal(f.messages.at(-1).following, true, 'downward movement reaches the one-pixel end tolerance');
    f.root.dataset.following = 'false'; f.notify();
    f.setHeights(() => 100); f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight, 'stale false cannot cancel resumed follow');
    f.root.dataset.following = 'true'; f.root.dataset.scrollRevision = '2'; f.notify();
});

test('a small native scrollbar move still counts as user input during streaming resize', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const before = f.offset('c:990');
    f.root.scrollTop -= 2;
    f.setHeights(index => index === 900 ? 2060 : 60);
    f.handlers.scroll(); f.flush();
    assert.equal(f.messages.at(-1).following, false);
    assert.equal(f.offset('c:990'), before + 2);
    f.notify();
    assert.equal(f.offset('c:990'), before + 2);
});

test('rapid wheel reversal ignores a delayed acknowledgement of the earlier intent', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.wheel(-5); f.wheel(5);
    assert.equal(f.root.scrollTop, end);
    assert.deepEqual(f.messages.map(message => [message.following, message.revision]), [[false, 1], [true, 2]]);
    f.root.dataset.following = 'false'; f.root.dataset.scrollRevision = '1'; f.notify();
    f.render(860, 1010); f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
});

test('explicit Follow and Ctrl-End revisions override unacknowledged local browsing', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    f.wheel(-9);
    f.root.dataset.followRevision = '1'; f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    f.wheel(-3);
    assert.equal(f.messages.at(-1).follow_revision, 1, 'later wheels belong to the new explicit follow generation');
    f.root.dataset.following = 'false'; f.root.dataset.scrollRevision = '2'; f.notify();
    f.root.dataset.followRevision = '2'; f.root.dataset.following = 'true'; f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
});

test('stale lower explicit revisions cannot roll back a newer follow generation or its wheel intent', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    f.root.dataset.followRevision = '2'; f.notify();
    const end = f.root.scrollTop;
    f.wheel(-3);
    f.root.dataset.followRevision = '1';
    f.root.dataset.following = 'true'; f.root.dataset.scrollRevision = '99'; f.notify();
    assert.equal(f.root.scrollTop, end - 3);
    f.wheel(-2);
    assert.equal(f.root.scrollTop, end - 5);
    assert.equal(f.messages[0].follow_revision, 2);
    f.root.dataset.followRevision = '3'; f.notify();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
});

test('explicit follow also supersedes a queued wheel whose native scroll event has not arrived', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    f.handlers['psa:transcript-user-scroll']({ detail: { deltaY: -3 } });
    f.root.scrollTop -= 3;
    f.root.dataset.followRevision = '1';
    f.flush();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    f.handlers.scroll(); f.flush();
    assert.equal(f.messages.length, 1, 'the obsolete scroll observation cannot emit a new follow cancellation');
});

test('native explicit Follow is accepted synchronously and a later wheel wins before any Rust paint', () => {
    const f = fixture({ start: 850 }); f.flush();
    assert.equal(f.follow(), true);
    const end = f.root.scrollHeight - f.root.clientHeight;
    assert.equal(f.root.scrollTop, end, 'accepted native follow moves immediately');
    assert.equal(f.root.dataset.followRevision, '0', 'no Rust acknowledgement has painted');
    f.wheel(-1);
    assert.equal(f.root.scrollTop, end - 1);
    assert.deepEqual(f.messages.map(message => [message.kind, message.revision, message.follow_revision]),
        [['follow', 1, undefined], ['scroll', 1, 1]]);
    f.root.dataset.followRevision = '1'; f.root.dataset.following = 'true'; f.notify();
    assert.equal(f.root.scrollTop, end - 1, 'an intermediate Follow acknowledgement cannot undo the later wheel');
    f.root.dataset.scrollRevision = '1'; f.root.dataset.following = 'false'; f.notify();
    assert.equal(f.root.scrollTop, end - 1);
});

test('older wheel, native Follow, and newer wheel retain their order through delayed Rust paints', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    const end = f.root.scrollTop;
    f.wheel(-2); assert.equal(f.follow(), true); f.wheel(-1);
    assert.deepEqual(f.messages.map(message => [message.kind, message.revision, message.follow_revision]),
        [['scroll', 1, 0], ['follow', 1, undefined], ['scroll', 2, 1]]);
    f.root.dataset.scrollRevision = '1'; f.root.dataset.following = 'false'; f.notify();
    assert.equal(f.root.scrollTop, end - 1, 'the earlier wheel acknowledgement is ignored');
    f.root.dataset.followRevision = '1'; f.root.dataset.following = 'true'; f.notify();
    assert.equal(f.root.scrollTop, end - 1, 'the intermediate explicit acknowledgement is ignored');
    f.root.dataset.scrollRevision = '2'; f.root.dataset.following = 'false'; f.notify();
    assert.equal(f.root.scrollTop, end - 1);
});

test('native explicit Follow accepts only a cancelable request on an active live viewport', () => {
    const f = fixture(); f.flush();
    f.root.dataset.active = 'false';
    assert.equal(f.follow(), false);
    f.root.dataset.active = 'true';
    f.handlers['psa:transcript-follow']({ cancelable: false,
        preventDefault() { assert.fail('a noncancelable request cannot be accepted'); } });
    assert.deepEqual(f.messages, []);
    assert.equal(f.follow(), true);
    assert.equal(f.follow(), true);
    assert.deepEqual(f.messages.map(message => [message.kind, message.revision]), [['follow', 1], ['follow', 2]]);
    f.controller.dispose();
    assert.equal(f.follow(), false, 'unmounted roots leave the original Rust shortcut/click intact');
});

test('programmatic follow, spacer replacement, resize and browser clamping do not look like user movement', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    let stopped = 0;
    const nativeScroll = () => f.handlers.scroll({ target: f.root, stopImmediatePropagation: () => stopped++ });
    nativeScroll(); f.flush();
    f.render(860, 1010); f.notify(); nativeScroll(); f.flush();
    f.setHeights(() => 100); f.notify(); nativeScroll(); f.flush();
    // A browser can clamp scrollTop before dispatching resize/scroll callbacks.
    f.setHeights(() => 20); f.root.scrollTop = f.root.scrollTop;
    nativeScroll(); f.flush();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - f.root.clientHeight);
    assert.deepEqual(f.messages, [], 'programmatic observation emits no user follow transitions');
    assert.equal(stopped, 4, 'native scroll events stay out of the renderer bridge');
});

test('continuous small upward wheels publish one coarse follow transition, not one message per frame', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    for (let index = 0; index < 30; index++) f.wheel(-1);
    assert.equal(f.messages.length, 1);
    assert.equal(f.messages[0].following, false);
});

test('repeated wheel intent at the history edge deduplicates requests until the page completes', () => {
    const f = fixture({ start: 0 }); f.root.dataset.hasOlder = 'true'; f.flush();
    f.scroll(0);
    const requests = () => f.messages.filter(message => message.kind === 'window' && message.history);
    assert.equal(requests().length, 1);
    for (let index = 0; index < 10; index++) f.wheel(-1);
    assert.equal(requests().length, 1);
    f.root.dataset.historyBusy = 'true'; f.notify();
    f.wheel(-1); assert.equal(requests().length, 1);
    f.root.dataset.historyBusy = 'false'; f.notify();
    assert.equal(requests().length, 2);
});

test('a normally scrollable history page smaller than the recycled window still requests older history', () => {
    const f = fixture({ start: 0, count: 100, following: true });
    f.root.dataset.hasOlder = 'true'; f.flush();
    f.scroll(0);
    const request = f.messages.at(-1);
    assert.equal(request.kind, 'window');
    assert.equal(request.history, true);
    assert.equal(request.metrics.top, 0);
    assert.equal(request.following, false);
    for (let index = 0; index < 10; index++) f.wheel(-1);
    assert.equal(f.messages.filter(message => message.kind === 'window').length, 1);
});

test('short invisible pages continue only after the previous history request finishes', () => {
    const f = fixture({ start: 0, count: 0 });
    f.root.dataset.hasOlder = 'true'; f.flush();
    assert.equal(f.messages.length, 1); assert.equal(f.messages[0].history, true);
    f.notify(); assert.equal(f.messages.length, 1);
    f.root.dataset.historyBusy = 'true'; f.notify(); assert.equal(f.messages.length, 1);
    f.root.dataset.historyBusy = 'false'; f.notify(); assert.equal(f.messages.length, 2);
    f.root.dataset.hasOlder = 'false'; f.notify(); assert.equal(f.messages.length, 2);
});

test('compact rows contribute their own gap, not the default, to spacer estimates', () => {
    const f = fixture(); f.flush();
    assert.equal(f.spacerTop(), 100 * 60);
    for (const row of f.mounted()) row.dataset.rowGap = '0';
    f.notify();
    assert.equal(f.spacerTop(), 100 * 48);
});

test('a requested jump waits for its row to mount, lands it below the top edge, and leaves follow', () => {
    const f = fixture({ start: 850, following: true }); f.flush();
    assert.equal(f.root.scrollTop, f.root.scrollHeight - 600);
    Object.assign(f.root.dataset, { following: 'false', followRevision: '1', jumpRevision: '1', jumpRow: 'c:300' });
    f.notify();
    f.render(225); f.notify();
    assert.equal(f.offset('c:300'), 12);
    assert.equal(f.messages.filter(message => message.kind === 'window').length, 0, 'the jumped-to row is mid-window');

    // A handled revision is not replayed by later layout work, and the row becomes the anchor.
    f.setHeights(index => index < 300 ? 90 : 60); f.notify();
    assert.equal(f.offset('c:300'), 12);
    f.wheel(40);
    assert.equal(f.offset('c:300'), -28);
});

test('a jump whose row never mounts expires, and user scrolling cancels a pending one', () => {
    const f = fixture(); f.flush();
    f.scroll(f.spacerTop() + 60 * 60);
    Object.assign(f.root.dataset, { jumpRevision: '1', jumpRow: 'c:900' });
    for (let pass = 0; pass < 4; pass++) f.notify();
    f.render(825); f.notify();
    assert.equal(f.offset('c:900') === 12, false, 'an expired jump must not fire later');

    const g = fixture(); g.flush();
    g.scroll(g.spacerTop() + 60 * 60);
    Object.assign(g.root.dataset, { jumpRevision: '1', jumpRow: 'c:240' });
    g.wheel(30, { scrollEvent: false });
    assert.equal(g.offset('c:160'), -30);
});

test('the navigator marks the last user message at or above the reading position', () => {
    const f = fixture(); f.flush();
    const entries = [100, 150, 150, 240].map(index => ({ dataset: { navIndex: String(index) } }));
    f.root.parentElement = { querySelectorAll: () => entries };
    const current = () => entries.map(entry => entry.dataset.current === 'true');
    f.scroll(f.spacerTop() + 60 * 60 + 9);
    assert.deepEqual(current(), [false, true, true, false]);
    f.scroll(f.spacerTop() + 40 * 60);
    assert.deepEqual(current(), [true, false, false, false]);
    assert.equal('current' in entries[1].dataset, false);
    f.scroll(f.spacerTop() + 138 * 60);
    assert.deepEqual(current(), [false, false, false, true]);
});

test('disposing removes observers/listeners, cancels queued frames, and releases the cache', () => {
    const f = fixture(); f.flush(); f.controller.refresh();
    f.controller.dispose(); f.controller.dispose();
    assert.equal(f.frames.size, 0);
    assert.deepEqual(Object.keys(f.handlers), []);
    assert.equal(f.observerDisposals(), 2);
});

test('the actual Dioxus eval bridge stays alive until disposal and cleans up observers', async () => {
    const f = fixture(); f.controller.dispose();
    let release, received = 0, settled = false, disconnected = 0;
    const disposal = new Promise(resolve => { release = resolve; });
    const dioxus = {
        recv: () => received++ === 0 ? Promise.resolve({ id: 'test', windowRows: 150 }) : disposal,
        send: () => {},
    };
    class Observer { observe() {} unobserve() {} disconnect() { disconnected++; } }
    const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
    const script = fs.readFileSync(path.join(__dirname, 'transcript_viewport.js'), 'utf8') + '\n'
        + fs.readFileSync(path.join(__dirname, 'transcript_viewport_bridge.js'), 'utf8');
    const evaluate = new AsyncFunction('document', 'dioxus', 'requestAnimationFrame', 'cancelAnimationFrame', 'ResizeObserver', 'MutationObserver', script);
    const running = evaluate({ getElementById: () => f.root }, dioxus, () => 1, () => {}, Observer, Observer).then(() => { settled = true; });
    await Promise.resolve(); await Promise.resolve();
    assert.equal(received, 2);
    assert.equal(settled, false, 'eval must not retire the channel while the viewport is mounted');
    release('dispose'); await running;
    assert.equal(disconnected, 2);
    assert.equal(f.handlers.scroll, undefined);
});
