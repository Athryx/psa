# Native file tools

Add `Read`, `Edit`, and `Write` to an agent definition's `tools` list to enable the
model-facing `read`, `edit`, and `write` tools. They are not installed implicitly.
Pi's behavior/prompt attribution notice is retained in `docs/PI_LICENSE.txt`.
For example:

```json
{"tools": ["Read", "Edit", "Write"]}
```

Tool definitions, directions, and rules are snapshotted by the configured
`ToolDefinitions` hook. Files are accessed on the session's probe, never on the
server's filesystem. Relative paths use the invocation's session working
directory. `@` prefixes, Unicode spaces, `file://` URLs, and `~` expansion follow
Pi's POSIX path conventions; home expansion uses the remote shell, not the
server's home directory. The remote system needs `sh` for home/existence checks.

## Compatibility scope

These tools port the model-facing behavior of the installed Pi implementation
(`dist/core/tools/read.js`, `edit.js`, `edit-diff.js`, `write.js`, and their image
helpers): tool names, description/direction/rule strings, argument descriptions,
ordinary success/error text, and text/image content blocks.

Pi also returns `details` and sometimes `structuredContent`. Those are separate
from its model-visible `content`: they drive UI diffs/truncation displays and
programmatic callers. This port deliberately does not add those fields to PSA's
`ToolOutput`; its existing text/image content list is sufficient for the selected
model-facing scope. PSA's typed-argument and cancellation error formatting remains
shared with other native tools.

### Read

- `path`, optional numeric `offset` (1-indexed), and optional numeric `limit`.
- Text uses Pi's head truncation: 2,000 complete lines or 50 KiB, whichever applies
  first. Continuation and oversized-first-line notices follow Pi. Middle
  truncation is the default for unspecified tools, not an override of this
  explicitly compatible read behavior.
- A fractional offset combined with an oversized first selected line produces
  the normal oversized-line notice; this does not preserve Pi's incidental
  JavaScript `Buffer.byteLength(undefined)` exception for that unusual input.
- Text input is streamed in two passes so output remains bounded while retaining
  total line counts. The file is not locked or snapshotted between passes; a
  concurrent writer can change it between the count and content reads.
- Image formats are detected from their bytes, not their extension. Supported
  images produce a text note followed by an inline image content block. Pi's
  animated-PNG/JPEG-LS signature exclusions are preserved.

### Image handling

Pi's default automatic image processing is used, without a new model-capability
registry:

1. Decode the image and account for EXIF orientation.
2. Convert BMP to PNG for inline attachment.
3. Preserve original encoded bytes if the image already fits the limits.
4. Otherwise fit the dimensions within 2,000 by 2,000 pixels, preserving aspect
   ratio. Try PNG, then JPEG qualities 80, 85, 70, 55, and 40. If none fits, reduce
   dimensions by 25 percent and retry, down to 1 by 1.
5. Require the **base64-encoded payload** to be smaller than 4.5 MiB, not merely
   the raw file. Base64 size is approximately four thirds of the raw byte size.
6. When resized, include the original/displayed dimensions and coordinate scale.
   For example, reducing 4,000 by 2,000 to 2,000 by 1,000 requires multiplying
   displayed coordinates by 2 to address the original image.

The note and MIME type can change when an image is converted/re-encoded. If
conversion/resizing fails, the result contains Pi's omission note rather than an
invalid image attachment. Native Rust codecs are used instead of Photon's WASM
codecs, so re-encoded bytes/pixels and compressed sizes can differ. The port does
not read model-specific image limits or add Pi's non-vision-model warning because
PSA currently exposes neither capability. Original images that fit are passed
through unchanged.

### Edit

- `path` and `edits: [{"oldText": "...", "newText": "..."}]`.
- Match every replacement against the original file, not incrementally.
- Preserve Pi's uniqueness/overlap checks, fuzzy text normalization, BOM and
  newline behavior, no-change errors, and compatibility input forms.
- Successful model-visible output is `Successfully replaced N block(s) in PATH.`
  Pi's UI diff, unified patch, and first-changed-line metadata are not emitted.

### Write

- `path` and `content`.
- Create parent directories recursively; create or overwrite the file.
- Successful output is `Successfully wrote to PATH`.

## Existing capability and safety boundaries

The port uses only existing probe operations. It does not introduce remote
permission/stat/realpath operations, native mutation transactions, or new runtime
APIs. Consequently this is not full filesystem/runtime parity with Pi:

- Shared tool-side locks order edit/write calls using the same probe ID and
  lexically normalized path. They do not coalesce symlink aliases or coordinate
  unrelated remote processes.
- Probe cancellation/disconnection does not acknowledge that underlying file IO
  has settled. A cancelled write may have side effects, including partial writes.
  PSA's normal bounded cancellation grace still applies. No atomicity or rollback
  is promised.
- Edit has no separate read/write-access preflight: permission failures may be
  observed later than in Pi. Known Linux errno messages are formatted like Node
  filesystem errors; unclassified probe failures retain their own message.
- Edit/image inputs have a 64 MiB in-memory input limit; text reads do not have
  that whole-file limit. Image decoding also retains the codec's 512 MiB default
  allocation limit. These safety bounds differ from Pi's whole-file reads.
- Dynamic error text is bounded with the shared UTF-8-safe middle truncation
  helper. Ordinary short success/error strings remain unchanged.

See `.agent/skills/making-tools/SKILL.md` for the authoring pattern and the rule to
stop and propose additions when a future tool needs unavailable capabilities.
