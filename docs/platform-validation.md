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
