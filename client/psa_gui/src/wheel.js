// Route wheel input by pointer position, never by keyboard focus. All movement
// stays in the browser; the transcript controller receives intent before moving.
function createHoveredWheelRouter(document, window) {
    const wheelStep = 72;
    const element = node => node?.nodeType === 1 ? node : node?.parentElement;
    const style = node => window.getComputedStyle(node);
    const scrollStyle = value => /^(auto|scroll|overlay)$/.test(value);
    const finite = value => Number.isFinite(value) ? value : 0;

    const bounds = (node, axis) => {
        if (axis === 'y') return [0, Math.max(0, node.scrollHeight - node.clientHeight)];
        const maximum = Math.max(0, node.scrollWidth - node.clientWidth);
        return style(node).direction === 'rtl' ? [-maximum, 0] : [0, maximum];
    };
    const position = (node, axis) => axis === 'y' ? node.scrollTop : node.scrollLeft;
    const canMove = (node, axis, delta) => {
        const [minimum, maximum] = bounds(node, axis);
        const current = position(node, axis);
        return maximum - minimum > 1 && (delta < 0 ? current > minimum + 0.5 : current < maximum - 0.5);
    };
    const candidates = (target, boundary, pane, dialog) => {
        const result = [];
        const add = node => {
            if (node && !result.includes(node)) result.push(node);
        };
        for (let node = target; node; node = node.parentElement) {
            const computed = style(node);
            if (scrollStyle(computed.overflowY) || scrollStyle(computed.overflowX)) add(node);
            if (node === boundary) break;
        }
        // Composer and tab chrome are siblings of the transcript, not ancestors.
        // A long textarea/popup still wins while it has room in this direction.
        if (pane) add(pane.querySelector('[data-active="true"] .transcript'));
        if (dialog) {
            add(dialog.querySelector('.settings-content'));
            add(dialog);
        }
        return result;
    };
    const supportsAxis = (node, axis) => {
        const computed = style(node);
        return scrollStyle(axis === 'y' ? computed.overflowY : computed.overflowX);
    };
    const choose = (nodes, axis, delta) => {
        if (!delta) return null;
        const ports = nodes.filter(node => supportsAxis(node, axis));
        return ports.find(node => canMove(node, axis, delta)) || ports.at(-1) || null;
    };
    const pixels = (delta, mode, legacyDelta, node, axis) => {
        if (!delta) return 0;
        let value = delta;
        if (mode === 1) {
            const computed = style(node);
            const line = parseFloat(computed.lineHeight) || (parseFloat(computed.fontSize) || 14) * 1.55;
            value *= Math.max(24, line);
        } else if (mode === 2) {
            value *= axis === 'y' ? node.clientHeight : node.clientWidth;
        }
        // Some WebKit/platform combinations report tiny pixel deltas for a
        // physical detent. The optional legacy value is only a detent hint;
        // precise/fractional trackpad deltas otherwise remain one-to-one.
        const notches = Math.abs(finite(legacyDelta)) / 120;
        if (mode !== 2 && notches >= 1 && Number.isInteger(notches)) {
            value = Math.sign(value) * Math.max(Math.abs(value), wheelStep * notches);
        }
        return value;
    };
    const handleWheel = event => {
        if (event.defaultPrevented || event.cancelable === false || event.ctrlKey || event.metaKey) return;
        // Read units before deltas: engines may adapt legacy delta access.
        const mode = event.deltaMode;
        let dx = finite(event.deltaX), dy = finite(event.deltaY);
        if (!dx && !dy) return;
        const hit = Number.isFinite(event.clientX) && Number.isFinite(event.clientY)
            ? document.elementFromPoint(event.clientX, event.clientY) : null;
        const target = element(hit) || element(event.target);
        if (!target?.closest('.shell')) return;
        const dialog = target.closest('[aria-modal="true"], dialog[open]');
        const backdrop = target.closest('.modal-backdrop');
        const pane = dialog || backdrop ? null : target.closest('.pane');
        const boundary = dialog || backdrop || pane || target.closest('.sidebar') || target.closest('.shell');
        const nodes = candidates(target, boundary, pane, dialog);
        let sourceX = 'x';
        const tabs = target.closest('.tabs');
        if (!dx && dy && (event.shiftKey || (tabs && tabs.scrollWidth > tabs.clientWidth + 1))) {
            dx = dy;
            dy = 0;
            sourceX = 'y';
        }
        const horizontal = choose(nodes, 'x', dx);
        const vertical = choose(nodes, 'y', dy);
        if (!horizontal && !vertical && !pane && !dialog && !backdrop && !target.closest('.sidebar')) return;
        event.preventDefault();
        event.stopPropagation();
        const legacyY = event.wheelDeltaY ?? event.wheelDelta;
        const moves = new Map();
        if (horizontal) moves.set(horizontal, { deltaX: pixels(dx, mode, sourceX === 'y' ? legacyY : event.wheelDeltaX, horizontal, 'x'), deltaY: 0 });
        if (vertical) {
            const movement = moves.get(vertical) || { deltaX: 0, deltaY: 0 };
            movement.deltaY = pixels(dy, mode, legacyY, vertical, 'y');
            moves.set(vertical, movement);
        }
        for (const [node, movement] of moves) {
            node.dispatchEvent(new window.CustomEvent('psa:transcript-user-scroll', { detail: movement }));
            if (movement.deltaX) {
                const [minimum, maximum] = bounds(node, 'x');
                node.scrollLeft = Math.max(minimum, Math.min(maximum, node.scrollLeft + movement.deltaX));
            }
            if (movement.deltaY) {
                const [minimum, maximum] = bounds(node, 'y');
                node.scrollTop = Math.max(minimum, Math.min(maximum, node.scrollTop + movement.deltaY));
            }
        }
    };
    const modalOpen = () => !!document.querySelector('[aria-modal="true"], dialog[open], .modal-backdrop');
    const requestFollow = root => root?.dispatchEvent(new window.CustomEvent('psa:transcript-follow', { cancelable: true })) === false;
    const consume = event => { event.preventDefault(); event.stopPropagation(); };
    const followKey = event => {
        if (event.defaultPrevented || event.key !== 'End' || !event.ctrlKey || event.altKey || event.metaKey || event.shiftKey || modalOpen()) return;
        const target = element(event.target);
        const pane = target?.closest('.pane') || document.querySelector('.pane.focused');
        if (requestFollow(pane?.querySelector('[data-active="true"] .transcript'))) consume(event);
    };
    const followClick = event => {
        if (event.defaultPrevented || event.button !== 0 || modalOpen()) return;
        const button = element(event.target)?.closest('.new-activity');
        if (button && requestFollow(button.closest('.transcript'))) consume(event);
    };
    document.addEventListener('wheel', handleWheel, { capture: true, passive: false });
    // Follow and subsequent wheel intent use the same viewport channel. Taking
    // these two explicit actions natively avoids a cross-channel/render race;
    // when no controller accepts them the ordinary Rust handlers still run.
    document.addEventListener('keydown', followKey, true);
    document.addEventListener('click', followClick, true);
    return {
        dispose() {
            document.removeEventListener('wheel', handleWheel, true);
            document.removeEventListener('keydown', followKey, true);
            document.removeEventListener('click', followClick, true);
        },
    };
}

if (typeof window !== 'undefined' && typeof document !== 'undefined' && !window.psaRemoveHoveredWheel) {
    const router = createHoveredWheelRouter(document, window);
    window.psaRemoveHoveredWheel = () => {
        router.dispose();
        delete window.psaRemoveHoveredWheel;
    };
}
if (typeof module !== 'undefined' && module.exports) module.exports = { createHoveredWheelRouter };
