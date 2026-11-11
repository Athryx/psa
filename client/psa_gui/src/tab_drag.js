// Pointer-driven tab dragging. Native HTML drag and drop is unreliable in the
// desktop WebView, so the browser tracks the drag locally and sends Rust one
// semantic drop: insert into a tab bar, join a pane, or split a pane's edge.
(() => {
    window.psaTabDrag?.dispose();

    const THRESHOLD = 5;
    // Within this fraction of a pane's size from an edge, the drop splits.
    const EDGE = 0.28;
    let press = null;
    let drag = null;

    function element(className) {
        const node = document.createElement('div');
        node.className = className;
        document.body.appendChild(node);
        return node;
    }

    function begin() {
        const tab = press.tab;
        const pane = tab.closest('[data-pane-id]');
        drag = {
            tab,
            view: Number(tab.dataset.tabId),
            sourcePane: pane ? Number(pane.dataset.paneId) : null,
            sourceTabs: pane ? pane.querySelectorAll(':scope > .tabs > .tab[data-tab-id]').length : 0,
            ghost: element('tab-drag-ghost'),
            preview: element('tab-drop-preview'),
            caret: element('tab-drop-caret'),
            target: null,
        };
        drag.ghost.textContent = tab.querySelector('.title')?.textContent ?? '';
        document.documentElement.classList.add('tab-dragging');
        tab.classList.add('drag-source');
    }

    function locateTabSlot(bar, paneId, x) {
        const bounds = bar.getBoundingClientRect();
        const tabs = Array.from(bar.querySelectorAll(':scope > .tab[data-tab-id]'));
        let slot = tabs.length;
        let caret = tabs.length ? tabs[tabs.length - 1].getBoundingClientRect().right : bounds.left;
        for (const [index, tab] of tabs.entries()) {
            const rect = tab.getBoundingClientRect();
            if (x < rect.left + rect.width / 2) {
                slot = index;
                caret = rect.left;
                break;
            }
        }
        return { kind: 'insert', pane: paneId, slot, caret: { x: caret, top: bounds.top, height: bounds.height } };
    }

    function locate(x, y) {
        const pane = document.elementFromPoint(x, y)?.closest('[data-pane-id]');
        if (!pane) return null;
        const paneId = Number(pane.dataset.paneId);
        const bar = pane.querySelector(':scope > .tabs');
        const paneRect = pane.getBoundingClientRect();
        const top = bar ? bar.getBoundingClientRect().bottom : paneRect.top;
        if (bar && y < top) return locateTabSlot(bar, paneId, x);

        const body = { left: paneRect.left, top, width: paneRect.width, height: paneRect.bottom - top };
        if (body.width <= 0 || body.height <= 0) return null;
        const fx = (x - body.left) / body.width;
        const fy = (y - body.top) / body.height;
        const [edge, distance] = [['left', fx], ['right', 1 - fx], ['top', fy], ['bottom', 1 - fy]]
            .sort((a, b) => a[1] - b[1])[0];
        const ownPane = paneId === drag.sourcePane;

        if (distance < EDGE) {
            // Splitting a pane by its only tab would just recreate the pane.
            if (ownPane && drag.sourceTabs <= 1) return null;
            const half = { ...body };
            if (edge === 'left' || edge === 'right') half.width /= 2;
            else half.height /= 2;
            if (edge === 'right') half.left += half.width;
            if (edge === 'bottom') half.top += half.height;
            return { kind: 'split', pane: paneId, edge, rect: half };
        }
        // The center of the tab's own pane leaves it where it was.
        if (ownPane) return null;
        return { kind: 'join', pane: paneId, rect: body };
    }

    function render(target) {
        const { preview, caret } = drag;
        if (target?.kind === 'insert') {
            caret.style.display = 'block';
            caret.style.transform = `translate(${target.caret.x - 1}px, ${target.caret.top + 4}px)`;
            caret.style.height = `${Math.max(0, target.caret.height - 8)}px`;
        } else {
            caret.style.display = 'none';
        }
        if (target?.rect) {
            const inset = 6;
            preview.style.display = 'block';
            preview.style.left = `${target.rect.left + inset}px`;
            preview.style.top = `${target.rect.top + inset}px`;
            preview.style.width = `${Math.max(0, target.rect.width - inset * 2)}px`;
            preview.style.height = `${Math.max(0, target.rect.height - inset * 2)}px`;
            preview.dataset.kind = target.kind;
        } else {
            preview.style.display = 'none';
        }
    }

    function finish() {
        if (drag) {
            drag.ghost.remove();
            drag.preview.remove();
            drag.caret.remove();
            drag.tab.classList.remove('drag-source');
            document.documentElement.classList.remove('tab-dragging');
        }
        press = null;
        drag = null;
    }

    // The click that ends a drag must not also activate the tab or its pane.
    function suppressNextClick() {
        const swallow = event => { event.preventDefault(); event.stopPropagation(); };
        window.addEventListener('click', swallow, { capture: true, once: true });
        setTimeout(() => window.removeEventListener('click', swallow, { capture: true }), 0);
    }

    function down(event) {
        if (event.button !== 0 || event.isPrimary === false || drag) return;
        const tab = event.target.closest?.('.tab[data-tab-id]');
        if (!tab || event.target.closest('.close')) return;
        press = { tab, id: event.pointerId, x: event.clientX, y: event.clientY };
    }

    function move(event) {
        if (!press || event.pointerId !== press.id) return;
        if (!drag) {
            if (Math.hypot(event.clientX - press.x, event.clientY - press.y) < THRESHOLD) return;
            begin();
        }
        event.preventDefault();
        drag.ghost.style.transform = `translate(${event.clientX + 14}px, ${event.clientY + 10}px)`;
        drag.target = locate(event.clientX, event.clientY);
        render(drag.target);
    }

    function up(event) {
        if (!press || event.pointerId !== press.id) return;
        const ended = drag;
        if (ended) {
            event.preventDefault();
            suppressNextClick();
        }
        finish();
        const target = ended?.target;
        if (!target) return;
        dioxus.send({ kind: target.kind, view: ended.view, pane: target.pane, slot: target.slot, edge: target.edge });
    }

    function key(event) {
        if (drag && event.key === 'Escape') {
            event.preventDefault();
            event.stopPropagation();
            finish();
        }
    }

    const listeners = [
        ['pointerdown', down],
        ['pointermove', move],
        ['pointerup', up],
        ['pointercancel', finish],
        ['keydown', key],
    ];
    for (const [name, callback] of listeners) document.addEventListener(name, callback, true);
    window.addEventListener('blur', finish);

    window.psaTabDrag = {
        dispose() {
            finish();
            for (const [name, callback] of listeners) document.removeEventListener(name, callback, true);
            window.removeEventListener('blur', finish);
        },
    };
})();
await new Promise(() => {});
