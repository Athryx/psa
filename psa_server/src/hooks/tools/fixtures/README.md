# Pi compatibility fixtures

These JSON files capture expected behavior from the locally installed
`@earendil-works/pi-coding-agent` tool implementations inspected for this port.
Tests consume the captured data directly; Node and Pi are not test dependencies.
Pi's attribution/license is retained in `docs/PI_LICENSE.txt` at the repo root.

- `pi_edit.json`: 120 cases obtained by stripping the initial BOM, applying
  `detectLineEnding`, `normalizeToLF`, `applyEditsToNormalizedContent`, and
  `restoreLineEndings`, then restoring the BOM, as Pi's `edit.js` does. Captures
  both returned file contents and thrown error messages. Cases include Unicode
  fuzzy normalization, original-coordinate disjoint edits, ambiguous/overlapping
  edits, no-ops, and LF/CRLF/CR/mixed newline variants.
- `pi_read.json`: 54 cases obtained from `createReadToolDefinition` with in-memory
  `access`/`readFile` operations and image detection disabled. Captures its first
  text content block or thrown error. Each input is `unit.repeat(repeat) + tail`;
  omitted `offset`/`limit` are stored as JSON null in the fixture only. Includes
  empty files, trailing newlines, offsets and limits, Unicode, line/byte limits,
  and continuation notices. Rust runs each case at multiple stream chunk sizes.
