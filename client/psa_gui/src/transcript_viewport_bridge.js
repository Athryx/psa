// Dioxus eval runs in an async function. Keep that function alive until cleanup;
// an un-awaited async IIFE would finish eval and retire its message channel.
const configuration = await dioxus.recv();
const root = document.getElementById(configuration.id);
if (!root) return;
const controller = createTranscriptViewport(root, message => dioxus.send(message), { windowRows: configuration.windowRows });
try {
    while (await dioxus.recv() !== 'dispose') controller.refresh();
} finally {
    controller.dispose();
}
