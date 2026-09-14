# Building and packaging Binwave

## Native Linux

The desktop application remains the default target:

```sh
cargo run --locked --release
cargo test --locked
```

A desktop session and graphics drivers are required. File pickers use the native
portal. Python decoders require Python 3 on PATH or `BINWAVE_PYTHON` pointing to
an interpreter. The Python runner is embedded in the executable.

## Native Windows x64

Install a stable Rust MSVC toolchain and Visual Studio Build Tools with the
**Desktop development with C++** workload and Windows SDK. From PowerShell:

```powershell
rustup target add x86_64-pc-windows-msvc
cargo test --locked
./scripts/package-windows.ps1
```

This builds `target/x86_64-pc-windows-msvc/release/binwave.exe` and creates a
portable folder, ZIP, and SHA-256 checksum in `dist/`. The package includes the
executable, public decoder/configuration examples, two small sample files, and
Python documentation. It does not bundle generated data or a Python interpreter.
The workflow uploads build artifacts only; it does not publish releases.

Extract the entire ZIP and launch `Binwave.exe` or `Launch-Binwave.cmd`.
Samples are discovered beside the executable even when launched from another
working directory. The release executable has no console window; Python output
still appears in the application's Console. YAML and Hex do not need Python.
Install Python 3 for scripts, or set `BINWAVE_PYTHON` to its executable. The MSVC
runtime must be available (install Microsoft's x64 Visual C++ Redistributable if
Windows reports a missing runtime DLL). This is an unsigned portable package, not
an installer; signing and installer distribution are outside this change.

Windows cache files avoid the read-only attribute so disposable entries can be
deleted; mappings remain read-only. Large native test fixtures explicitly enable
NTFS sparse files before setting their logical lengths.

Native source/destination comparisons now use cross-platform file identity to
reject hard links as export destinations. Existing native Linux behavior and
64-bit desktop decoding remain unchanged. Windows Python cancellation terminates
the interpreter; descendant-process cleanup is not equivalent to Unix process
group cleanup and remains a limitation.

## Browser WebAssembly preview

Install the WASM target and Trunk (the workflow pins Trunk 0.21.14):

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked trunk --version 0.21.14
trunk serve
# Open http://127.0.0.1:8080
```

Build static assets without deploying:

```sh
trunk build --release --locked
# Output: dist/web/
```

Serve the whole directory over HTTP on localhost, or HTTPS on a host of your
choice; opening `index.html` via `file://` is unsupported. Relative asset paths
support subdirectory hosting. The server must serve `.wasm` as
`application/wasm` and `.mjs` as JavaScript. No application backend, accounts,
telemetry, Python service, cross-origin isolation, or shared WASM memory is used.
Do not omit the `web/` worker files when copying the output.

The browser version uses eframe/egui, egui_plot, the existing shared pane model
and dock layout/persistence, and shared byte display conventions. It has a
separate asynchronous adapter and bounded hex view; it does **not** yet reuse
all of the desktop `BinwaveApp` or `HexEditor`. YAML interpretation, numeric plot
planning/reduction, adaptive traces, and the syntax-highlighted source editor are
shared with native code. See [architecture boundaries](architecture.md).

### Available in this preview

- Open a user-selected local file without reading it in full.
- Inspect a 64 KiB page of hex/text, jump to decimal or hexadecimal absolute
  offsets, and move between pages. Scrolling is virtualized within the page.
- Use Open decoder to select a local YAML file (up to 256 KiB), or edit YAML
  directly; tap Apply or press F5. The decoder picker works independently of the
  data-file picker and uses a standard file input on LAN HTTP.
- Inspect decoded structures and 128-record sample pages. Selecting a decoded field selects its byte range in Hex.
- Plot numeric channels from a decoded collection with the native min/max and
  sampled overview algorithm. Collections initially display 512 samples. Detail
  returns to a 512-sample window; Overview covers the collection. Zoom/pan reloads
  the visible sample range after gestures settle. Explicit ranges remain
  available. Quality labels distinguish sampled amplitude bands from traces.
- Select and replace individual bytes, undo/redo, and extract an explicit range
  whose end offset is exclusive. Edits remain worker-owned sparse patches.
- Stream an edited copy or extraction to a user-selected destination, with
  cancellation and progress. Export awaits each write and commits at close;
  read/write/validation failures abort the writable stream.
- Rearrange and persist dock panels. Unsaved file edits are not session backups;
  opening a new file prompts before discarding them and unloading requests the
  browser's standard unsaved-work warning.

### Memory and offset contract

The worker retains the `File`/file handle, not a copy of its contents. Every read
uses bounded `File.slice` ranges. Decoding uses `FileReaderSync` in a dedicated
worker, with four cached 64 KiB pages and a 1 MiB read ceiling. The UI holds only
one 64 KiB page; export buffers are at most 1 MiB, with one write in flight.
There is no whole-file `File.arrayBuffer()`, Blob download fallback, memory map,
full-file cache. Explicit YAML actions may scan data in bounded chunks; cancelling
terminates the decoder worker. Plot overviews inspect at most 65,536 records. The bridge briefly represents the bounded
page as JSON for WASM transfer; this adds bounded temporary overhead. One file
operation runs at a time and worker progress is throttled. Read errors require
an explicit retry/navigation; no automatic retry loop repeatedly reads a broken
source.

Rust file positions are `u64`; worker positions and patch keys are `BigInt`.
Messages serialize offsets as decimal strings, never WASM `usize`. Conversion to
JavaScript Number occurs only for Blob's range API, after validation. Files or
positions above `2^53 - 1` are explicitly rejected because the browser API cannot
address them exactly. Offsets above 4 GiB work independently of wasm32 memory.
Native adapters also share the bounded-range validator; converting the rest of
the legacy native decoder's `usize` offsets remains future work.

Edits are limited to 65,536 touched byte positions and 4,096 undo steps. Undo does
not remove positions from the touched-position budget, and dirty tracking after
undo is conservative. Saving a complete copy clears dirty state; extracting a
subrange does not. No file data or patch history is stored in localStorage.

### Compatibility and missing features

The egui workspace uses wgpu: WebGPU where available, with WebGL2 fallback.
An embedded browser may expose neither API; use a regular browser window in that
case. A graphics startup failure displays recovery instructions and disables file
opening. There is no CPU-only renderer. No browser security flags are changed.

Safe streaming export requires `showOpenFilePicker`, `showSaveFilePicker`, and
writable file handles (use a supporting desktop Chromium browser on HTTPS or
localhost). The pickers run from ordinary DOM buttons to preserve browser user
activation. Browsers without these APIs can open a file through a file input and
inspect/edit it, but export is disabled; the application never substitutes a
whole-file in-memory download. The fallback explicitly reports this limitation.

Exports reject the same file-system entry as the open source. Browsers expose
less identity information than native OS APIs: arbitrary hard-link aliases or
same-size changes with preserved timestamps cannot reliably be identified.
Source size and modification time are checked before reads and before/after
export; browsers can also invalidate reads when a source changes. Keep source
files unchanged while working.

CSV, native Python, full-file Hex searching, multi-byte replacement,
and continuous whole-file hex scrolling are not implemented
in the browser yet. Python cannot run as a native subprocess in a browser. Byte
edits invalidate decoded results; apply YAML again to decode the new revision.
Failed YAML applies retain the previous successful worker session and results.
Source drafts are persisted up to 256 KiB; file contents and edits are not.

## Validation

See the [recorded local validation results](platform-validation.md) for the checks
actually run and the outstanding Windows/browser smoke tests.

```sh
node tests/browser_backend.mjs
cargo check --locked --target wasm32-unknown-unknown
cargo test --locked
```

The Node tests use a synthetic 40 GiB file whose whole-file reader throws. They
verify bounded tail reads, >4 GiB edits, exact offset limits, streaming with
backpressure, undo/redo caps, source protection, cancellation, and aborts on
source changes/write failures. They do not substitute for a browser picker and
filesystem permission smoke test or a real Windows launch.

Before distributing a build, smoke-test Windows launch/open/decoder/save and
browser open/jump/edit/undo/export/reopen, including a multi-gigabyte source,
source overwrite rejection, export cancellation, empty files, reload guards,
and a browser without file-system picker support.

References: [Trunk asset handling](https://github.com/trunk-rs/trunk/blob/main/guide/src/assets/index.md),
[File system handles](https://developer.mozilla.org/en-US/docs/Web/API/FileSystemFileHandle),
[stream writes](https://developer.mozilla.org/en-US/docs/Web/API/FileSystemWritableFileStream/write).

## Product rename compatibility

Binwave is the new application, executable, and Python SDK name. Saved scripts
using `from plottermax import ...` still work through a compatibility alias.
`BINWAVE_PYTHON` and `BINWAVE_CACHE_DIR` take precedence over their legacy
`PLOTTERMAX_` equivalents. Existing cache directories and settings keys are retained
to preserve user state. The generated worktree folder name is independent of the
application name and may still contain `plottermaxx`.
