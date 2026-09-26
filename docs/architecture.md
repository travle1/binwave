# Native and browser boundaries

The platform split is at file access and job execution. Adding browser support
must not introduce another decoder or a second plot-reduction algorithm.

| Concern | Shared implementation | Platform adapter |
| --- | --- | --- |
| YAML syntax, execution, lazy arrays, numeric plans | `structure.rs` | Native `ByteView`; browser bounded worker reader |
| Viewport sampling, exact reads, min/max reduction | `structure_plot.rs` | Native thread scheduler; browser decoder worker |
| Dense trace rendering | `plot_trace.rs` | Both shells use `decoded_channel`; sampled bands never connect unread gaps |
| Source editor, highlighting, shortcuts, templates | `source_editor.rs` | Native and browser language/draft controls |
| Inspector field lookup, display, and numeric encoding | `structure_selection.rs`, `workspace_inspector.rs`, `field_edit.rs` | Native selection and browser on-demand worker requests |
| Python SDK and plan generation | `python/runner.py` | Native subprocess; browser-local Pyodide worker |
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
CSV, native file dialogs, and OS caching remain platform-specific gaps.
Python uses the same runner and decoder plan on both platforms, but the browser
loads a pinned Pyodide runtime into a worker on first use; it does not start a
native subprocess. Browser scripts can make network requests and are not a
security sandbox. The browser needs a separate file/worker adapter, not another
decoder implementation.

Native numeric plotting now routes eligible ordinary and grouped projections
through one tiled reader. Its decoded RAM tiles and extrema summaries use
hash lookups with LRU eviction; concurrent requests for one tile share the
in-flight load. A separate native-only decoded-chunk cache can restore covered
tiles after RAM eviction or restart. Chunks carry source/plan identity and a
payload checksum; invalid chunks fall back to the source. The optional Keep
setting protects a file's chunks from ordinary eviction and cache clearing.
The browser continues to use the shared decoder/reducer without native
filesystem persistence. A persistent multilevel summary index and a global
native resource scheduler remain planned work.
Medium-density CPU raster preparation runs on one cancellable background job.
The UI retains the last texture at its original plot coordinates while a
compatible viewport redraw is pending; obsolete results are discarded.

Validation combines native engine tests with the actual built WASM API against
a synthetic 40 GiB file (`tests/browser_decoder.mjs`). The synthetic file forbids
whole-file reads and validates bounded plots, offsets, edits, and failed applies.
