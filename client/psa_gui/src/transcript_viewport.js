// The DOM is a fixed-size window. Only lightweight height metadata survives
// recycling. Anchors are stable event IDs plus a pixel offset, never percentages
// or the total scroll-height delta (which includes unrelated live activity).
function createTranscriptViewport(root, send, environment = {}) {
    const scheduleFrame = environment.requestAnimationFrame || requestAnimationFrame;
    const cancelFrame = environment.cancelAnimationFrame || cancelAnimationFrame;
    const Resize = environment.ResizeObserver || ResizeObserver;
    const Mutations = environment.MutationObserver || MutationObserver;
    const limit = environment.windowRows || 150;
    const measurements = new Map();
    const cacheLimit = 3000;
    let average = 80, anchor = null, frame = null, disposed = false;
    let previousFirst = null, previousCount = 0, previousBusy = false, pending = null;
    const observed = new Set();
    let adjusting = false, userScrollTop = null;
    let following = root.dataset.following === 'true';
    let followRevision = Number(root.dataset.followRevision || 0);
    let scrollRevision = Number(root.dataset.scrollRevision || 0);
    const metrics = () => ({ height: root.scrollHeight, top: root.scrollTop, client_height: root.clientHeight });
    let lastScroll = metrics();
    let nearTop = lastScroll.top <= 80;
    const syncFollowing = state => {
        const revision = Number(root.dataset.followRevision || 0);
        // Explicit Follow/Ctrl-End wins even before Rust acknowledges an earlier
        // wheel. Otherwise an old data-following value cannot undo local intent.
        if (revision < followRevision) return;
        if (revision > followRevision) {
            following = state.following;
            userScrollTop = null;
            pending = null;
            lastScroll = metrics();
            nearTop = lastScroll.top <= 80;
        } else if (Number(root.dataset.scrollRevision || 0) >= scrollRevision) {
            following = state.following;
        }
        followRevision = revision;
    };
    const publishScroll = () => send({ kind: 'scroll', following, revision: scrollRevision,
        follow_revision: followRevision, metrics: metrics() });
    const setFollowing = value => {
        if (following === value) return;
        following = value;
        scrollRevision++;
        publishScroll();
    };
    const atEnd = value => value.height - value.client_height - value.top <= 1;
    const topSpacer = () => root.querySelector('[data-transcript-spacer="top"]');
    const bottomSpacer = () => root.querySelector('[data-transcript-spacer="bottom"]');
    const rows = () => Array.from(root.querySelectorAll('[data-transcript-row]'));
    const read = () => ({
        start: Number(root.dataset.windowStart), end: Number(root.dataset.windowEnd),
        count: Number(root.dataset.rowCount), first: root.dataset.firstRow,
        active: root.dataset.active === 'true', following: root.dataset.following === 'true',
        hasOlder: root.dataset.hasOlder === 'true', historyBusy: root.dataset.historyBusy === 'true',
    });
    const viewportTop = () => root.getBoundingClientRect().top + (root.clientTop || 0);
    const capture = mounted => {
        const top = viewportTop();
        const row = mounted.find(row => row.getBoundingClientRect().bottom > top);
        if (!row) return null;
        const rect = row.getBoundingClientRect();
        // A thumb jump can land wholly inside a spacer. Such a jump has no
        // mounted content anchor and must not be pulled back to the old window.
        if (rect.top > top + root.clientHeight) return null;
        return { id: row.dataset.transcriptRow, index: Number(row.dataset.rowIndex), offset: rect.top - top };
    };
    const heightBefore = end => {
        let height = end * average;
        for (const [index, measurement] of measurements) {
            if (index < end) height += measurement.height - average;
        }
        return Math.max(0, height);
    };
    const request = (start, state, history = false) => {
        const key = `${state.first}:${state.count}:${start}:${history}`;
        if (pending === key) return;
        pending = key;
        send({ kind: 'window', start, first: state.first, count: state.count, history,
            following, follow_revision: followRevision, metrics: metrics() });
    };
    const clampStart = (start, state) => Math.max(0, Math.min(start, Math.max(0, state.count - limit)));
    const observeScroll = () => {
        const current = metrics();
        const origin = userScrollTop ?? lastScroll.top;
        const moved = current.top !== origin;
        const layoutChanged = current.height !== lastScroll.height || current.client_height !== lastScroll.client_height;
        const wasFollowing = following;
        // With overflow anchoring disabled, layout alone can move scrollTop only
        // by clamping to the new end. An away-from-end thumb move is still user
        // input when a streamed row happens to resize in the same frame.
        const layoutClamp = layoutChanged && atEnd(current);
        if (moved && (!layoutClamp || userScrollTop !== null)) {
            // Also covers native scrollbar dragging, keyboard, and touch moves.
            setFollowing(current.top > origin && atEnd(current));
            if (layoutChanged && anchor) {
                anchor.offset -= current.top - origin;
            } else anchor = capture(rows());
        }
        userScrollTop = null;
        if (nearTop !== (current.top <= 80) && following === wasFollowing) publishScroll();
        nearTop = current.top <= 80;
        lastScroll = current;
    };
    const refresh = () => {
        frame = null;
        if (disposed) return;
        const state = read();
        if (!state.active) { pending = null; return; }
        syncFollowing(state);
        // A routed wheel can be applied before its asynchronous scroll event.
        // Capture that position before a queued resize/recycler frame restores.
        observeScroll();
        const mounted = rows();
        const changed = previousFirst !== state.first || previousCount !== state.count;
        if (changed || previousBusy !== state.historyBusy) pending = null;
        previousBusy = state.historyBusy;

        // History prepends move ordinal positions, but never event identities.
        // Shift cached ordinals using an overlapping event before measuring the
        // new window. Only the bounded measurement cache is visited.
        if (previousCount !== state.count) {
            let shift = 0;
            for (const row of mounted) {
                const previous = Array.from(measurements).find(([, value]) => value.id === row.dataset.transcriptRow);
                if (previous) { shift = Number(row.dataset.rowIndex) - previous[0]; break; }
            }
            if (shift) {
                const shifted = Array.from(measurements, ([index, value]) => [index + shift, value]);
                measurements.clear();
                for (const [index, value] of shifted) if (index >= 0 && index < state.count) measurements.set(index, value);
            }
        }
        // An absent row in a mounted raw-event range is intentionally hidden,
        // not an unknown eighty-pixel row. This enables invisible-page loading.
        for (let index = state.start; index < state.end; index++) {
            measurements.delete(index); // Refresh the bounded metadata LRU.
            measurements.set(index, { id: null, height: 0 });
        }
        let totalHeight = 0;
        for (const row of mounted) {
            const height = row.getBoundingClientRect().height + 12;
            measurements.set(Number(row.dataset.rowIndex), { id: row.dataset.transcriptRow, height });
            totalHeight += height;
        }
        if (mounted.length) average = Math.max(1, totalHeight / mounted.length);
        while (measurements.size > cacheLimit) measurements.delete(measurements.keys().next().value);
        const top = heightBefore(state.start);
        const bottom = Math.max(0, heightBefore(state.count) - heightBefore(state.end));
        const topNode = topSpacer(), bottomNode = bottomSpacer();
        if (topNode) topNode.style.height = `${top}px`;
        if (bottomNode) bottomNode.style.height = `${bottom}px`;

        adjusting = true;
        if (following) {
            root.scrollTop = root.scrollHeight;
        } else if (anchor) {
            const row = mounted.find(row => row.dataset.transcriptRow === anchor.id);
            if (row) root.scrollTop += row.getBoundingClientRect().top - viewportTop() - anchor.offset;
        }
        adjusting = false;
        // Browser scroll events are asynchronous. Remember the actual (clamped)
        // result, not just a synchronous adjusting flag, to recognize our writes.
        lastScroll = metrics();
        nearTop = lastScroll.top <= 80;
        previousFirst = state.first;
        previousCount = state.count;
        const visible = capture(mounted);
        anchor = visible;
        for (const row of observed) {
            if (!mounted.includes(row)) { resize.unobserve(row); observed.delete(row); }
        }
        for (const row of mounted) {
            if (!observed.has(row)) { resize.observe(row); observed.add(row); }
        }

        if (state.historyBusy) return;
        if (!mounted.length && (state.start > 0 || state.hasOlder)) {
            request(clampStart(state.start - limit / 2, state), state, true);
            return;
        }
        if (root.scrollHeight <= root.clientHeight + 1 && (state.start > 0 || state.hasOlder)) {
            request(clampStart(state.start - limit / 2, state), state, true);
            return;
        }
        if (!following && state.start === 0 && root.scrollTop <= 80 && state.hasOlder) {
            request(0, state, true);
            return;
        }
        if (following || state.count <= limit) return;
        let index;
        if (visible) index = visible.index;
        else {
            // Approximate only the destination of a scrollbar jump. Subsequent
            // measured windows replace estimates, preserving actual row anchors.
            const y = root.scrollTop;
            let low = 0, high = state.count;
            while (low < high) {
                const middle = Math.floor((low + high) / 2);
                if (heightBefore(middle + 1) < y) low = middle + 1;
                else high = middle;
            }
            index = low;
        }
        const nearEdge = !visible || index < state.start + 30 || index >= state.end - 30;
        const next = clampStart(index - Math.floor(limit / 2), state);
        if (nearEdge && next !== state.start) request(next, state);
    };
    const schedule = () => {
        if (!disposed && frame === null) frame = scheduleFrame(refresh);
    };
    const onScroll = event => {
        if (event?.target && event.target !== root) return;
        // Keep high-frequency native events out of the renderer bridge. Rust
        // receives only follow changes, top-zone crossings, and window requests.
        event?.stopImmediatePropagation?.();
        if (adjusting || root.dataset.active !== 'true') return;
        syncFollowing(read());
        observeScroll();
        schedule();
    };
    const onUserScroll = event => {
        if (root.dataset.active !== 'true') return;
        syncFollowing(read());
        const delta = Number(event.detail?.deltaY || 0);
        if (delta !== 0) {
            userScrollTop = root.scrollTop;
            anchor = capture(rows());
        }
        // The hovered-wheel router dispatches this BEFORE changing scrollTop.
        // No Rust roundtrip (or eighty-pixel threshold) may swallow a small step.
        if (delta < 0) setFollowing(false);
        else if (delta > 0 && atEnd(metrics())) setFollowing(true);
        schedule();
    };
    const onFollow = event => {
        if (disposed || !event.cancelable || root.dataset.active !== 'true') return;
        // The router consumes the original key/click only when a live viewport
        // accepts it. Follow and later wheels then share one ordered channel.
        event.preventDefault();
        syncFollowing(read());
        followRevision++;
        following = true;
        userScrollTop = null;
        pending = null;
        anchor = null;
        adjusting = true;
        root.scrollTop = root.scrollHeight;
        adjusting = false;
        lastScroll = metrics();
        nearTop = lastScroll.top <= 80;
        send({ kind: 'follow', revision: followRevision, metrics: lastScroll });
        schedule();
    };
    const onWheel = event => {
        if (!event.ctrlKey && !event.metaKey) onUserScroll({ detail: { deltaY: event.deltaY } });
    };
    // Resize must preserve the previously captured row even when an earlier
    // expanded detail, wrapping font, or pane width changes its position.
    const resize = new Resize(schedule);
    const mutations = new Mutations(schedule);
    mutations.observe(root, { childList: true, subtree: true, characterData: true, attributes: true,
        attributeFilter: ['data-window-start', 'data-window-end', 'data-row-count', 'data-first-row', 'data-active', 'data-following', 'data-follow-revision', 'data-scroll-revision', 'data-has-older', 'data-history-busy', 'open'] });
    root.addEventListener('scroll', onScroll, { passive: true, capture: true });
    root.addEventListener('psa:transcript-user-scroll', onUserScroll);
    root.addEventListener('psa:transcript-follow', onFollow);
    root.addEventListener('wheel', onWheel, { passive: true, capture: true });
    resize.observe(root);
    schedule();
    return {
        refresh: schedule,
        dispose() {
            if (disposed) return;
            disposed = true;
            if (frame !== null) cancelFrame(frame);
            resize.disconnect();
            mutations.disconnect();
            root.removeEventListener('scroll', onScroll, true);
            root.removeEventListener('psa:transcript-user-scroll', onUserScroll);
            root.removeEventListener('psa:transcript-follow', onFollow);
            root.removeEventListener('wheel', onWheel, true);
            measurements.clear();
        },
    };
}

if (typeof module !== 'undefined' && module.exports) {
    module.exports = { createTranscriptViewport };
}
