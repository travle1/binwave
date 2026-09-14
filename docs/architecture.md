# Native and browser boundaries

The platform split is at file access and job execution. Adding browser support
must not introduce another decoder or a second plot-reduction algorithm.

| Concern | Shared implementation | Platform adapter |
| --- | --- | --- |
| YAML syntax, execution, lazy arrays, numeric plans | `structure.rs` | Native `ByteView`; browser bounded worker reader |
| Viewport sampling, exact reads, min/max reduction | `structure_plot.rs` | Native thread scheduler; browser decoder worker |
| Dense trace rendering | `plot_trace.rs` | Both shells use `decoded_channel`; sampled bands never connect unread gaps |
| Source editor, highlighting, shortcuts, templates | `source_editor.rs` | Native language controls; browser YAML controls |
| Dock arrangement and persistence | `workspace_layout.rs` | Native and browser application state |
| File identity, dialogs, patches, streaming export | Shared range validation; platform-specific I/O | Native OS files versus browser handles/workers |

`browser_decoder.rs` exposes bounded JSON requests around the shared engines.
`browser_bytes.rs` supplies the same read interface using worker-local synchronous
slice reads; it never copies a whole file into WASM. `decoder-worker.mjs` loads
the same WASM build as the UI, with GUI startup skipped in a worker. The outer
file worker owns edits and file validation. New decoder sessions replace old
ones only after successful execution and source validation. Cancelling a new
apply terminates that candidate worker; the previous session remains available.

Browser file offsets and record counts are u64, serialized as decimal strings
and converted to BigInt in the file adapter. The shared decoder's transitional
`Address` alias is u64 on wasm and usize on native, retaining existing native
interfaces. Memory-buffer indices are checked or bounded before usize conversion.
The browser Blob API limits addressable offsets to 2^53−1. Moving the remaining
native interfaces to u64 is still a separate migration.

The browser shell still duplicates some view composition and Hex interactions.
It is not full native UI parity. Future work should extract platform-independent
controllers/views where useful, rather than copy native decoders into the browser.
CSV, Python execution, native file dialogs, and OS caching are not abstracted by
pretending browser APIs have the same capabilities.

Validation combines native engine tests with the actual built WASM API against
a synthetic 40 GiB file (`tests/browser_decoder.mjs`). The synthetic file forbids
whole-file reads and validates bounded plots, offsets, edits, and failed applies.
