// Pointer focus happens on click, not mousedown, so dragging to select text is
// never interrupted. Explicit/open requests wait for a mounted, enabled prompt;
// subsequent interaction always wins over a delayed render or network response.
if (!window.psaRemovePaneFocus) {
    let pending = null;
    const interactive = 'a, button, input, textarea, select, option, label, summary, details, [contenteditable]:not([contenteditable="false"]), [role="button"], [role="link"], [role="dialog"], [tabindex]';
    const modalOpen = () => !!document.querySelector('[aria-modal="true"], dialog[open], .modal-backdrop');
    const composer = pane => pane.querySelector('[data-active="true"] .composer textarea:not(:disabled)');
    const selection = () => {
        const pane = document.querySelector('.pane.focused');
        const active = pane?.querySelector('[data-active="true"][data-view-id]');
        return [pane?.getAttribute('data-pane-id'), active?.getAttribute('data-view-id')].join(':');
    };
    const cancel = () => {
        if (!pending) return;
        pending.observer.disconnect();
        clearTimeout(pending.timeout);
        if (pending.frame !== null) cancelAnimationFrame(pending.frame);
        pending = null;
    };
    const attempt = request => {
        if (pending !== request) return;
        if (!request.shell.isConnected || modalOpen()) return cancel();
        if (request.view === null) {
            // The DOM can commit an async open before its Rust effect reaches
            // JS. Remember the transition and validate it once its view is known.
            const current = selection();
            if (current !== request.baseline && !current.endsWith(':')) {
                if (request.transition !== null && request.transition !== current) return cancel();
                request.transition = current;
            }
            return;
        }
        const pane = document.querySelector(`.pane.focused[data-pane-id="${request.group}"]`);
        const active = pane?.querySelector(`[data-active="true"][data-view-id="${request.view}"]`);
        if (!active) {
            // The old selection may remain until Dioxus commits this open, but
            // a third view (or leaving a view already seen) invalidates it.
            const current = selection();
            if (request.active || (current !== request.baseline && current !== `${request.group}:` && current !== ':')) cancel();
            return;
        }
        if (request.active && request.active !== active) return cancel();
        request.active = active;
        const input = active.querySelector('.composer textarea:not(:disabled)');
        if (!input) return;
        cancel();
        input.focus({ preventScroll: true });
    };
    const begin = token => {
        cancel();
        const shell = document.getElementById('psa-shell');
        if (!shell || modalOpen()) return null;
        const request = { token, shell, baseline: selection(), transition: null, group: null, view: null, active: null, frame: null };
        request.observer = new MutationObserver(() => attempt(request));
        request.observer.observe(document.documentElement, {
            subtree: true, childList: true, attributes: true,
            attributeFilter: ['disabled', 'data-active', 'data-view-id', 'data-pane-id', 'class', 'open', 'aria-modal'],
        });
        request.timeout = setTimeout(() => { if (pending === request) cancel(); }, 10000);
        pending = request;
        return request;
    };
    const click = event => {
        if (event.button !== 0 || event.defaultPrevented || modalOpen()) return;
        const target = event.target instanceof Element ? event.target : event.target.parentElement;
        const pane = target?.closest('.pane');
        if (!pane || target.closest(interactive) || !window.getSelection()?.isCollapsed) return;
        composer(pane)?.focus({ preventScroll: true });
    };
    // An async open captures intent before its first await. A late response
    // cannot create fresh intent after a pointer/focus/modal/view change.
    window.psaBeginComposerOpen = token => { begin(token); };
    window.psaFocusComposer = (group, view, token) => {
        const request = token === undefined ? begin(null) : pending;
        if (!request || (token !== undefined && request.token !== token)) return;
        if (request.transition !== null && request.transition !== `${group}:${view}`) return cancel();
        request.group = group;
        request.view = view;
        if (request.frame !== null) cancelAnimationFrame(request.frame);
        request.frame = requestAnimationFrame(() => {
            request.frame = null;
            attempt(request);
        });
    };
    window.addEventListener('blur', cancel);
    document.addEventListener('pointerdown', cancel, true);
    document.addEventListener('focusin', cancel, true);
    document.addEventListener('click', click);
    window.psaRemovePaneFocus = () => {
        cancel();
        window.removeEventListener('blur', cancel);
        document.removeEventListener('pointerdown', cancel, true);
        document.removeEventListener('focusin', cancel, true);
        document.removeEventListener('click', click);
        delete window.psaBeginComposerOpen;
        delete window.psaFocusComposer;
        delete window.psaRemovePaneFocus;
    };
}
