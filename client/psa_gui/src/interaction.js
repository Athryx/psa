// One installation per document, shared by independently mounted controls.
// Native input/pointer events never cross the renderer's delegated listeners.
(() => {
    if (window.psaInteractions) return;

    const controls = new Map();
    let observer = null;
    const clamp = (value, min, max) => Math.min(max, Math.max(min, value));
    const movementKey = key => ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown'].includes(key);
    const stop = event => event.stopImmediatePropagation();

    function mount(token, options, publish, done, kind) {
        remove(token);
        const element = document.getElementById(options.id);
        if (!element) { done(); return; }

        let config = options;
        let value = Number(options.value);
        let committed = value;
        let revision = 0;
        let timer = null;
        let pointer = null;
        let disposed = false;
        let preview = null;
        let previewFrame = null;
        let branches = null;
        const heldKeys = new Set();
        const listeners = [];
        const pointerListeners = [];

        function listen(target, name, callback, transient = false) {
            target.addEventListener(name, callback, true);
            (transient ? pointerListeners : listeners).push([target, name, callback]);
        }

        function removeListeners(list) {
            for (const [target, name, callback] of list) target.removeEventListener(name, callback, true);
            list.length = 0;
        }

        function clearPreview() {
            if (previewFrame !== null) cancelAnimationFrame(previewFrame);
            previewFrame = null;
            if (!preview) return;
            const { shell, property, previous, priority, current } = preview;
            if (shell.style.getPropertyValue(property) === current) {
                if (previous) shell.style.setProperty(property, previous, priority);
                else shell.style.removeProperty(property);
            }
            preview = null;
        }

        function showPreview() {
            const property = config.preview_property;
            if (typeof property !== 'string' || !property.startsWith('--')) return;
            const shell = element.closest('.shell');
            if (!shell) return;
            if (preview && (preview.shell !== shell || preview.property !== property)) clearPreview();
            if (!preview) preview = {
                shell, property,
                previous: shell.style.getPropertyValue(property),
                priority: shell.style.getPropertyPriority(property),
            };
            // Each preview re-lays out the whole shell (font size, widths), so
            // apply at most one per frame instead of one per input event.
            if (previewFrame !== null) return;
            previewFrame = requestAnimationFrame(() => {
                previewFrame = null;
                if (!preview) return;
                preview.current = `${value}${config.preview_unit || ''}`;
                shell.style.setProperty(property, preview.current);
            });
        }

        function findBranches() {
            const parent = document.getElementById(config.split_id);
            if (!parent || element.parentElement !== parent || !parent.classList.contains('split')) return null;
            const siblings = Array.from(parent.children);
            const index = siblings.indexOf(element);
            const first = siblings[index - 1], second = siblings[index + 1];
            if (!first?.classList.contains('branch') || !second?.classList.contains('branch')) return null;
            return { parent, first, second };
        }

        function apply(next, previewValue = true) {
            if (!Number.isFinite(next)) return;
            value = clamp(next, Number(config.min), Number(config.max));
            if (kind === 'range') {
                // The input remains uncontrolled after mount. Only an explicit,
                // acknowledged external update may write its live value.
                if (element.value !== String(value)) element.value = String(value);
                if (previewValue) showPreview();
            } else if (kind === 'edge') {
                const target = document.getElementById(config.target_id);
                if (!target) return;
                target.style.width = `${value}px`;
                element.setAttribute('aria-valuenow', String(Math.round(value)));
            } else {
                branches = findBranches();
                if (!branches) return;
                branches.first.style.flex = `${value} 1 0%`;
                branches.second.style.flex = `${1 - value} 1 0%`;
                element.setAttribute('aria-valuenow', String(Math.round(value * 100)));
            }
        }

        function clearTimer() {
            if (timer !== null) window.clearTimeout(timer);
            timer = null;
        }

        function commit() {
            clearTimer();
            if (disposed || !element.isConnected || value === committed) return;
            committed = value;
            publish({ value, revision: ++revision });
        }

        function coalesce() {
            clearTimer();
            // A continuous pointer drag is local, regardless of its duration.
            if (!pointer) timer = window.setTimeout(commit, 120);
        }

        function readRange() {
            const next = Number(element.value);
            if (Number.isFinite(next)) apply(next);
        }

        function move(event) {
            if (!pointer || event.pointerId !== pointer.id || kind === 'range') return;
            const coordinate = config.axis === 'vertical' ? event.clientY : event.clientX;
            if (!Number.isFinite(coordinate)) return;
            event.preventDefault();
            stop(event);
            apply(pointer.value + (coordinate - pointer.coordinate) / pointer.size);
        }

        function finish(event, finalCommit = true) {
            if (!pointer || (event && event.pointerId !== undefined && event.pointerId !== pointer.id)) return;
            // Pointerup can contain the final coordinate without a preceding move.
            if (event?.type === 'pointerup') move(event);
            if (kind === 'range') readRange();
            const id = pointer.id;
            pointer = null;
            removeListeners(pointerListeners);
            try {
                if (element.hasPointerCapture(id)) element.releasePointerCapture(id);
            } catch (_) { /* Capture may already be lost or the node removed. */ }
            if (finalCommit) commit();
        }

        function start(event) {
            if (pointer || event.button !== 0 || event.isPrimary === false) return;
            if (kind === 'divider') {
                branches = findBranches();
                if (!branches) return;
                const rect = branches.parent.getBoundingClientRect();
                const handle = element.getBoundingClientRect();
                const vertical = config.axis === 'vertical';
                const size = (vertical ? rect.height - handle.height : rect.width - handle.width);
                const coordinate = vertical ? event.clientY : event.clientX;
                if (!Number.isFinite(size) || size <= 0 || !Number.isFinite(coordinate)) return;
                event.preventDefault();
                stop(event);
                element.focus({ preventScroll: true });
                pointer = { id: event.pointerId, coordinate, size, value };
                listen(window, 'pointermove', move, true);
            } else if (kind === 'edge') {
                // Pixel widths follow the pointer one-to-one along the x axis.
                if (!document.getElementById(config.target_id) || !Number.isFinite(event.clientX)) return;
                event.preventDefault();
                stop(event);
                element.focus({ preventScroll: true });
                pointer = { id: event.pointerId, coordinate: event.clientX, size: 1, value };
                listen(window, 'pointermove', move, true);
            } else {
                // Do not prevent the range's native thumb/track behavior.
                pointer = { id: event.pointerId };
            }
            clearTimer();
            listen(window, 'pointerup', finish, true);
            listen(window, 'pointercancel', finish, true);
            listen(window, 'blur', event => { if (event.target === window) finish(null); }, true);
            // A native range already tracks its own thumb drag; capturing the
            // pointer on the input can stop WebKit from moving the thumb.
            if (kind !== 'range') {
                try { element.setPointerCapture(event.pointerId); } catch (_) { /* Window listeners remain a fallback. */ }
            }
        }

        function keydown(event) {
            if (event.altKey || event.ctrlKey || event.metaKey) return;
            if (kind === 'range') {
                if (movementKey(event.key)) {
                    heldKeys.add(event.key);
                    stop(event); // Native default action is retained.
                }
                return;
            }
            const vertical = config.axis === 'vertical';
            const delta = kind === 'edge' ? (event.shiftKey ? 32 : 8) : (event.shiftKey ? 0.1 : 0.02);
            let next;
            if (event.key === 'Home') next = Number(config.min);
            else if (event.key === 'End') next = Number(config.max);
            else if (event.key === (vertical ? 'ArrowUp' : 'ArrowLeft')) next = value - delta;
            else if (event.key === (vertical ? 'ArrowDown' : 'ArrowRight')) next = value + delta;
            else return;
            event.preventDefault();
            stop(event);
            apply(next);
            coalesce();
        }

        listen(element, 'pointerdown', start);
        listen(element, 'lostpointercapture', finish);
        listen(element, 'keydown', keydown);
        listen(element, 'keyup', event => {
            if (!movementKey(event.key) || event.altKey || event.ctrlKey || event.metaKey) return;
            stop(event);
            heldKeys.delete(event.key);
            if (kind === 'range') readRange();
            if (!pointer && !heldKeys.size) commit();
        });
        listen(element, 'blur', () => { heldKeys.clear(); if (!pointer) commit(); });
        if (kind === 'range') {
            listen(element, 'input', event => { stop(event); readRange(); coalesce(); });
            listen(element, 'change', event => {
                stop(event);
                readRange();
                // Browsers emit change for every repeated range keydown, not
                // just keyup. Preserve that native behavior without forwarding
                // every repetition to Rust.
                if (!pointer) coalesce();
            });
        }

        controls.set(token, {
            element,
            update(next, acknowledgedRevision) {
                const changedIdentity = config.split_id !== next.split_id || config.axis !== next.axis
                    || config.path !== next.path || config.target_id !== next.target_id;
                if (changedIdentity) {
                    finish(null, false);
                    clearTimer();
                    committed = Number(next.value);
                    config = next;
                    apply(committed, false);
                    clearPreview();
                    return;
                }
                config = next;
                // A delayed Rust echo must not rewind a newer local interaction.
                // Reapply local styles too: the renderer may have patched the
                // branch styles before sending this semantic acknowledgement.
                if (pointer || value !== committed || acknowledgedRevision < revision) {
                    apply(value);
                    return;
                }
                clearTimer();
                committed = Number(next.value);
                apply(committed, false);
                clearPreview();
            },
            dispose() {
                disposed = true;
                clearTimer();
                finish(null, false);
                removeListeners(pointerListeners);
                removeListeners(listeners);
                clearPreview();
                done();
            },
        });
        apply(value, false);
        if (!observer) {
            observer = new MutationObserver(() => {
                for (const [key, control] of controls) if (!control.element.isConnected) remove(key);
            });
            observer.observe(document.documentElement, { childList: true, subtree: true });
        }
    }

    function remove(token) {
        const control = controls.get(token);
        if (!control) return;
        controls.delete(token);
        control.dispose();
        if (!controls.size && observer) { observer.disconnect(); observer = null; }
    }

    window.psaInteractions = {
        range: (token, options, publish, done) => mount(token, options, publish, done, 'range'),
        divider: (token, options, publish, done) => mount(token, options, publish, done, 'divider'),
        edge: (token, options, publish, done) => mount(token, options, publish, done, 'edge'),
        update: (token, options, revision) => controls.get(token)?.update(options, revision),
        remove,
    };
})();
