# Platform implementation validation

Checked in the isolated `0dea/plottermaxx` worktree on September 13, 2026.
The main repository was renamed to `/home/developer/projects/rust/binwave` while
work was in progress; the Git worktree pointer was repaired afterward.

| Check | Result |
| --- | --- |
| Native Linux `cargo check` | Passed |
| Native Linux `cargo test --locked --offline` with a writable temporary cache | 322 unit tests and 42 integration tests passed; 12 existing tests ignored |
| Windows GNU `cargo check --locked --offline --tests --target x86_64-pc-windows-gnu` | Passed, including Windows-specific test fixture code |
| WASM `cargo check --locked --target wasm32-unknown-unknown` | Passed |
| WASM release build | Passed |
| Trunk 0.21.14 `build --release --locked` | Passed; static assets in `dist/web`, approximately 3.2 MiB WASM |
| `node tests/browser_backend.mjs` | 8 tests passed |
| Worker/bridge JavaScript syntax checks | Passed |
| `git diff --check` | Passed |

The first native test attempt hit the sandbox's read-only application cache.
Rerunning with cache access passed; a later full run used
`PLOTTERMAX_CACHE_DIR=/tmp/plottermax-test-cache` and passed too. No tests were
removed or newly ignored to obtain these results.

Trunk's locked build dependency failed with the host's GCC 16; installing Trunk
with the already-installed GCC 15 succeeded. This was a local tooling issue,
not a change to the application. The host also sets `NO_COLOR=1`, which this Trunk
version expects as a boolean; local Trunk validation used `env -u NO_COLOR`.

The Windows check was cross-compilation on Linux, not a Windows executable link
or runtime test. The MSVC packaging script and Windows CI job still need to run
on a Windows host. No Windows ZIP has been generated or launched locally.

The in-app browser refused localhost access because it could not verify an
admin-enforced security policy. Visual startup, native browser pickers, filesystem
permissions, and a real browser export were therefore **not** smoke-tested. This
restriction was not bypassed. The Node tests exercise the worker's backend using
bounded synthetic sources and writable sink mocks, not browser permission UI.

The browser remains a preview. Decoder parity (YAML/CSV/Python), full desktop
Hex interaction parity, and the remaining native `usize` decoder migrations are
not complete. See [platform scope and limitations](platforms.md).

No deployment, release publication, or Git push was performed.

## Graphics startup follow-up

A user smoke test reported `WebGL isn't supported` in the embedded browser.
The WASM renderer now uses wgpu with WebGPU and WebGL2 fallback instead of
WebGL-only glow. Startup failures show a recovery panel; file opening stays
disabled until the workspace initializes. Two Node tests cover the startup gate
and failure UI using a simulated DOM. These tests do not verify real GPU support.

The follow-up Trunk release build passed; the rebuilt WASM is approximately
5.2 MiB with both wgpu backends included. Real rendering in the embedded browser
remains unverified.

## Binwave rename

The user confirmed browser graphics acceleration was disabled. The application,
Cargo binary, Windows package, browser bridge, Python SDK, and documentation now
use Binwave. Legacy Python imports and environment variables remain aliases;
persistent storage identifiers are retained. The renamed build passed 364 Rust
tests, 12 Python tests (including the legacy SDK alias), both JavaScript test
suites, the Windows test cross-check, and the Trunk release build.

## Shared browser decoding and plots

After checkpoint `5e9bc61`, the browser now compiles the native YAML engine,
lazy array reader, plot sampling/min/max reducer, adaptive trace renderer, and
source editor. The UI supports YAML apply, decoded structures, paged samples,
field-to-byte selection, numeric channels, and explicit plot-range refinement.

Validation of this slice:

- `BINWAVE_CACHE_DIR=/tmp/binwave-test-cache cargo test --locked`: 322 unit and
  42 integration tests passed; 12 existing ignored tests. The first run without
  a writable cache had one read-only-filesystem failure; the cache override
  resolved it without changing application code.
- `cargo check --locked --tests --target x86_64-pc-windows-gnu`: passed. This is
  a cross-check, not a Windows executable launch or MSVC package test.
- Trunk release build: passed.
- Existing browser backend and startup tests: passed.
- Two new worker lifecycle tests: passed (failed-apply retention, cancellation,
  source-edit invalidation, and sparse patch forwarding beyond 4 GiB).
- Actual generated WASM API executed in Node against a synthetic 40 GiB file:
  YAML, lazy offsets, patched values, table paging, exact plots, bounded overview,
  and failed-apply retention passed. Only 129 bounded slices were read across
  the test; whole-file reads are forbidden by the fixture.
- localhost:8080 returned HTTP 200. The updated UI has not been visually verified
  through automation; embedded browser control was unavailable. User browser
  smoke testing remains necessary, including real file picker/export behavior.

The browser remains short of full native parity; see `platforms.md` and
`architecture.md` for the current boundaries. No deployment or push was performed.

## Decoder picker and sampled overview correction

Added a standard decoder file input, bounded to 256 KiB and independent of the
data-file picker. Five startup/picker tests pass, including cancellation, failed
reads, and rejecting oversized/Python inputs before reading them.

The browser had dropped `PlotData.envelopes` and rendered sampled overviews as
continuous traces. It now transports those bands and both native and browser
shells call the same `plot_trace::decoded_channel` renderer. Sampled overview
bands never join individual observations across unread gaps. Exact and min/max
traces retain their existing adaptive rendering.

Validation: 322 native unit tests pass, including overview shape assertions;
Trunk release succeeds; actual WASM tests pass against both the synthetic 40 GiB
source and an 8 MB fixture from the repository waveform generator. The real YAML
schema produces twelve channels, the expected sine at 0/16/48, 384 exact samples,
and 128 estimated bands spanning −4 to +4. The generated-fixture check also runs
in browser CI. The LAN server serves the rebuilt output; real browser visual
verification remains with the user.
