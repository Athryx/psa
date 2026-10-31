// Pointer focus happens on click, not mousedown, so dragging to select text is
// never interrupted. Keyboard requests are deferred until Dioxus renders the
// newly active pane, and invalidated by any subsequent pointer interaction.
if (!window.psaRemovePaneFocus) {
    let generation = 0;
    const interactive = 'a, button, input, textarea, select, option, label, summary, details, [contenteditable]:not([contenteditable="false"]), [role="button"], [role="link"], [role="dialog"], [tabindex]';
    const modalOpen = () => !!document.querySelector('[aria-modal="true"], dialog[open], .modal-backdrop');
    const composer = pane => pane.querySelector('[data-active="true"] .composer textarea:not(:disabled)');
    const pointerDown = () => { generation++; };
    const click = event => {
        if (event.button !== 0 || event.defaultPrevented || modalOpen()) return;
        const target = event.target instanceof Element ? event.target : event.target.parentElement;
        const pane = target?.closest('.pane');
        if (!pane || target.closest(interactive) || !window.getSelection()?.isCollapsed) return;
        composer(pane)?.focus({ preventScroll: true });
    };
    window.psaFocusComposer = (group, view) => {
        const request = ++generation;
        requestAnimationFrame(() => {
            if (generation !== request || modalOpen()) return;
            const pane = document.querySelector(`.pane.focused[data-pane-id="${group}"]`);
            const active = pane?.querySelector(`[data-active="true"][data-view-id="${view}"]`);
            active?.querySelector('.composer textarea:not(:disabled)')?.focus({ preventScroll: true });
        });
    };
    document.addEventListener('pointerdown', pointerDown, true);
    document.addEventListener('focusin', pointerDown, true);
    document.addEventListener('click', click);
    window.psaRemovePaneFocus = () => {
        generation++;
        document.removeEventListener('pointerdown', pointerDown, true);
        document.removeEventListener('focusin', pointerDown, true);
        document.removeEventListener('click', click);
        delete window.psaFocusComposer;
        delete window.psaRemovePaneFocus;
    };
}
