# BinWave

A local Rust desktop workspace using eframe/egui and egui_plot. Inspect and edit
bytes from any file, write Python or YAML decoders using reusable data structures,
and plot large binary and CSV datasets. No backend, accounts, telemetry, or runtime AI.

## Launch

Use a recent stable Rust toolchain (Rust 1.88 or newer) and a desktop session:

```sh
cargo run --release
```

Linux needs working X11 or Wayland and graphics drivers; native file pickers use
the desktop file portal. Open and save pickers keep the workspace responding while
you browse. Compilation may download crates; file processing is local.
Python decoders require an installed Python 3 interpreter; YAML and Hex do not.

Sample files are grouped by purpose: `samples/data/`,
`samples/decoders/python/`, `samples/decoders/yaml/`, `samples/configs/`, and
`samples/generators/`. See the [sample directory guide](samples/README.md).

Each open/save dialog remembers its own folder across restarts: data files, YAML,
Python, CSV configurations, and byte exports stay separate. Choose a directory once;
cancelled or failed operations leave its remembered location unchanged. Initial
folders are `samples/data/`, `samples/decoders/yaml/`, `samples/decoders/python/`,
and `samples/configs/` respectively. Exports start in the current working directory
until the first successful export.

## Browser port constraint

The application currently runs as a native desktop program. A future browser
backend must read bounded file ranges and stream exports; it must never require
loading a whole data file into WebAssembly or JavaScript memory. File offsets must
remain 64-bit independently of the memory address size, with bounded caches and
background work to keep the interface responsive.

## Editing workspace

**File → Open file…** (Ctrl+O) or drop a file to open one shared editing workspace. Every extension,
including `.csv`, `.bin`, `.png`, `.zip`, `.yaml`, `.md`, and extensionless files,
opens as bytes. No definition or full-file scan is required to start using Hex.
Opening an arbitrary file never applies the demo layout.

Before opening a file, the workspace puts **Decoder** on the left and **Hex** on
the right. Opening a nonempty binary file adds a **Byte preview** plot above Hex:
unsigned 8-bit values at byte offset zero, one byte per sample. The first 1,024
values appear through bounded reads; more are loaded as you navigate. The file
is not scanned or copied into RAM to create this preview.

Untouched YAML and Python starters become short, executable definitions of that
byte interpretation. Edit the fields, byte order, or starting offset and press
F5 to replace the preview with your decoded data. The built-in preview uses the
native YAML reader; opening a data file never runs user Python. Loaded, edited,
and deliberately cleared decoder drafts remain intact, and the plot stays labeled
**Byte preview** until a different decoder successfully replaces it.

The decoder starts in **YAML**, with **Python** and **CSV** alongside it.
Python and YAML retain independent source drafts when you switch modes; switching
does not convert or execute either source. Choosing an editor preserves the current
Plot and Decoded samples, including their selection and view. Apply or run the
chosen decoder to replace those results.
**Open decoder…** selects Python for `.py` and YAML for `.yaml` or `.yml`, then
loads that source without executing it.

For Python, choose **Run script** (F5); **Stop** cancels a running script and
**Save Python…** saves its source. For YAML, choose **Decode file** (F5) and
**Save YAML…**. Run executes the active draft against the open file. A successful
run opens **Decoded samples** below Hex; **Show plot** opens the decoded channels. The decoder stays visible beside results.
With no data file open, both source editors start with editable, commented examples;
**New** restores the
starter for the selected language. The examples read a sample count from a header,
then describe that many records. Adapt the fields to your file, select all, and
press Ctrl+/ (Cmd+/ on macOS) to uncomment. The same shortcut toggles comments on
selected lines while editing. Untouched starters are clean drafts and cannot run
until they contain uncommented code. Opening a decoder replaces its language's
starter with the loaded source. Ordinary scripts can finish
without publishing a dataset; output appears in Console, and existing results
remain available. Only a successful data-producing run replaces them.
Opening CSV selects its settings in Decoder and automatically shows a plot and
table from an initial portion of the numeric columns. More rows are read when requested; opening does not scan to EOF.

Python is a direct source editor with a console for completed runs. For YAML,
the source editor and **Fields** and **Steps** forms edit the same YAML draft.
Fields creates a format from selected bytes, edits an existing format, or reuses one at another location;
Steps controls reading order and conditions. **Decoded structure** is the result
tree, and **Inspector** shows the selected value and its editing controls. These
supporting views share the sidebar without reserving more columns.

Drag panel titles to split, stack, or float views, and drag borders to resize them.
Existing arrangements persist; **View → Decoder workspace** selects the new default.
**View → Plot and bytes workspace** keeps Decoder beside a plot and Hex. Closing a
panel preserves its contents, selection, edits, and history. Open files, unsaved
edits, and named byte ranges are not session backups.

### Hex operations

Hex shows continuously scrolling, virtualized rows of offsets, hex bytes, and text.
It fills the available height and fits 8, 16, or 32 bytes per row to the panel width;
**View** also allows an explicit row width. Reads run in background workers and
remain bounded regardless of file size. Scrolling continues through large files
without exposing internal read windows or requiring separate paging controls.

- Click or drag to select bytes; Shift-click or Shift-arrow extends the range.
  Arrow keys, Home/End, Page Up/Down, and Ctrl/Cmd-Home/End move through the file.
  Hex and text share the same selection.
- **Jump** accepts an absolute byte offset (`256` or `0x100`) or a signed distance
  from the cursor (`+256`, `-16`, or `+0x100`). Press Enter or **Go**. **Edit → Go to
  byte…** (Ctrl+G) opens Hex and focuses this input. Invalid or out-of-file jumps
  leave your position unchanged. **Back** and **Forward** revisit locations; they
  are disabled when there is no location in that direction.
- **Find** searches hex byte pairs or text in a cancellable worker. **Previous** and
  **Next** visit highlighted matches. Searches retain at most 10,000 matches and
  accept patterns up to 4,096 bytes; stopping retains the matches already found.
- **Selection** and the byte context menu provide **Copy hex**, **Copy text**,
  **Extract selection…**, **Replace bytes…**, and **Copy Python read** (or
  **Describe selected bytes…** for YAML). Clipboard copies are limited to 1 MiB; extraction streams
  larger ranges directly to a chosen output file.
- Enable **Overwrite**, then type hex pairs into selected bytes. Click the text
  column or enable **Text** to enter UTF-8 text. Double-clicking a byte also enables
  overwrite mode. **Replace bytes…** requires exactly as many replacement bytes
  as the selected range; one replacement is limited to 1 MiB. These operations
  preserve file length. Escape exits overwrite mode; Undo/Redo restore byte edits.
- **Name / color range…** adds a bookmark without any YAML. Revisit or remove it
  under **View → Saved ranges**. Parsed fields add their own range colors and
  boundaries; hover a byte to inspect overlapping structure names and offsets.
  Selection forms a continuous band across Hex and text. Muted field tints show
  only the selected sample; search matches and modified bytes use separate edge
  marks. Hover identifies the narrowest decoded field at that byte.
- **Inspector** shows the selected range, integer and floating-point interpretations
  in both byte orders, text, and the first byte's bits. Selecting a decoded element
  reveals its bytes; selecting bytes identifies covering decoded elements.

### Shared edits and saving

Every interpretation uses one working byte view. Raw edits are sparse replacements
and undo groups over the original file; opening and editing do not copy the entire
file or allocate its size in RAM. Changed bytes stay distinguishable from original
bytes, including after saving a copy. Byte edits invalidate dependent decoded views
and trigger background reinterpretation of the last applied decoder. For Python,
this reruns the last script that successfully published decoded data against the updated working bytes; unrun source
changes in the editor remain a draft.

**Save edited copy…** streams the complete working file to a different destination.
**Extract selection…** streams only the selected bytes. Existing destinations
require confirmation; saving over the open source, or a link to it, is rejected.
Failed saves preserve the destination. An extraction does not mark the whole file
saved. Undo/Redo history remains shared across byte and legacy numeric edits;
changing a decoder preserves that byte history. Unsaved bytes, Python/YAML source,
and pending legacy table edits are guarded when replacing the open file or closing
BinWave.

`.csv` files read an initial portion automatically while Hex remains available.
Select **CSV** in Decoder and press **Apply** for delimited numeric text with another extension.
Decoder holds CSV settings, Plot holds column visibility and navigation, and
Inspector shows the selected bytes or decoded value. Raw CSV edits clear CSV
results because that reader accesses the original file; save and reopen the
edited copy to plot its updated CSV values. CSV table cells are read-only; selecting
a value reveals its exact source bytes in Hex and marks its sample in Plot.
Selecting a row number selects the entire raw record, including its line ending.

General Open treats Python, YAML, and Markdown as data files too. Use
**Open decoder…** in Python or YAML mode for a saved script or definition, or
**Open configuration…** in CSV mode for a Markdown configuration. The latter is
also available as **File → Load CSV configuration…**.
**Tools → Clear disk cache** removes disposable entries without closing the file.

## Python decoders

Python decoders use ordinary Python imports, functions, loops, and installed
libraries. BinWave supplies `file` for reading the open file, including unsaved
byte edits, and `Struct`/`Field` for describing byte-backed results:

```python
from binwave import Struct

Header = Struct("Header", [("count", "u16"), ("offset", "u32")])
Sample = Struct("Sample", [("value", "i16")])

header = file.read_struct("header", Header)
samples = file.records("samples", Sample, count=header.count, offset=header.offset)
print(f"{len(samples):,} samples")
```

Assignments, imports, calculations, `print()`, and raw byte reads work without
registering decoded data. For example, `test = 5` finishes successfully with no
printed output; add `print(test)` to see `5` in Console. Each Run starts a fresh
Python interpreter, so keep assignments and the code using them in the same script.
Errors and stopped runs preserve results from the last successful decoder for the
same byte revision.

`read_struct()` returns metadata you can use in normal Python control flow.
`records()` registers a typed collection for Plot, Decoded samples, and Hex without
loading every record. Python emits an internal plan of typed file ranges that the
shared results engine reads lazily. YAML describes schemas and reading operations
for that same engine. Sharing results does not translate Python source into YAML,
or YAML source into Python. `file.read()`, `read_at()`, and `unpack()` support freeform
inspection and calculations; they do not automatically create displayed results.
The current output API describes fixed-size structures over source bytes. It does
not register arbitrary computed arrays or Python callbacks for every plotted sample.

Output and tracebacks appear in the console when execution finishes. **Stop** can
cancel a script, including a Python loop. Loaded scripts use their own directory
for relative imports and paths. The runtime uses `python3` (`python` on Windows);
set `BINWAVE_PYTHON` to a virtual environment's interpreter to use its libraries.
Scripts run with your local user's permissions and are **not sandboxed**.

See the [Python decoder API](python/README.md) and the
[Waveform Python example](samples/decoders/python/waveform.py), which discovers sample fields
from embedded descriptors and registers the 40 GB sample collection lazily.

## YAML decoders and data-driven interpretation

A definition is independent of the file extension. Named **structures** describe
fields and nested elements; **actions** read them, choose interpretations, skip or
align bytes, calculate values, or construct further structures from decoded data.
The application does not reserve domain-specific names such as channel or section:
those names and their meaning belong to your YAML.

For guided editing, select **YAML**, choose **New**, then start with a selection:

1. Select bytes in Hex. In Decoder → **Fields**, choose **New from selection** (or use **Describe selected bytes…** from the Hex selection menu).
2. Give the selection one **Name**, such as `header`. Name each field and choose
   its format. Its byte size, position, and live **Value** appear underneath. Add
   fields as needed; the byte-order menu changes how numbers are interpreted.
   The usage indicator shows how much of your selection is described: choosing a
   16-bit field in a 15-byte selection leaves 13 bytes to describe.
3. Click **Decode selection**. The description runs immediately and opens
   **Decoded samples**. Click a row to select a sample's bytes, or a cell to select
   one field. Keep the form open to correct a format or field, then click
   **Decode selection** again to update the same description.
4. Select another range in Hex and use the format selector in **Fields** to
   reuse a saved format there. Select an existing format to edit its fields in
   place. **Save YAML…** keeps the decoder for other files.

For repeated records, **Decode all … records in this selection** appears when the
selection contains an exact number of fixed-size records. **Advanced** contains
absolute file position, repeat count, per-record byte bounds, and the reusable
format name. Each field's **...** menu provides reordering, deletion, count, and
absolute position. Leave those settings alone when the fields simply follow one
another.

Build on values you have already decoded:

- Select a decoded number, then choose **Inspector → Use this value for…** to
  supply a count, byte length, or file offset. References use earlier decoded
  values, such as `header.count`.
- Select a decoded collection, then choose **Inspector → Build a type from these
  descriptions…**. Map each entry's name and format to a field; optional mappings
  cover type codes, counts, byte lengths, and relative positions. The resulting
  format can decode later data. This form opens from the collection it describes.

**Steps** and **Source** stay available while editing fields.
Steps provides forms for **Read data**, **Skip bytes**, **Align position**,
**Calculate a value**, **Build type from descriptions**, and **Choose by value**,
including earlier-value choices and nested branches. Both views edit the same
source; run their changes with F5. These forms produce YAML; Python is available
when you need ordinary functions, loops, and imports.

**Decode → Examples** loads working starting points, and **Help → Decoding guide**
explains the selection workflow. Saved definitions may include unfinished drafts;
syntax is checked when decoding. Legacy layouts remain editable in YAML; save and
clear a legacy draft before starting a generic definition.

For example, this definition reads a header, then uses its values to locate and
size a collection without scanning its payload first:

```yaml
endian: little
structures:
  Header:
    fields:
      - {name: count, type: u16}
      - {name: offset, type: u32}
  Value:
    fields:
      - {name: sample, type: i16}
actions:
  - read: {name: header, type: Header}
  - read:
      name: values
      type: Value
      at: {ref: header.offset}
      repeat: {ref: header.count}
```

The more involved [generic section example](samples/decoders/yaml/generic_sections.yaml) defines
an ordinary reusable structure containing an ID, a length, and a bounded payload.
A choice based on the ID reads descriptions from one payload and generates a new
type. A later payload uses that discovered type. The definitions may be nested;
there is no hardcoded header or descriptor format. The
[generic definition reference](samples/generic_structures.md) includes complete
synthetic bytes, supported actions, reference rules, and examples.

The interpreter supports exact integer values, standard fixed-width numeric types,
UTF-8 text, zero-terminated text, raw byte regions, and unsigned LEB128 `varuint`.
Named calculations use checked `add`, `subtract`, `multiply`, and exact `divide`
operations. This is a declarative reader with controlled operations; YAML cannot
execute arbitrary code or access unrelated files.

### Formats and current boundaries

These examples exercise reusable mechanisms; they are not automatic file importers:

| Format | Available definition building blocks | Current boundary |
| --- | --- | --- |
| PNG | [Chunk example](samples/decoders/yaml/png_chunks.yaml): signature, big-endian length, text type, bounded payload, and IHDR interpretation. | Image data remains raw compressed bytes; CRC is displayed, not verified or rebuilt. |
| RIFF/WAVE | [Container example](samples/decoders/yaml/riff_chunks.yaml): nested bounded regions, type choices, format metadata, and two-byte alignment. | Audio payload and unknown chunks remain raw bytes; there is no audio decoder. |
| ZIP | Offset references, variable lengths, and type choices can describe directory entries and referenced records. | No complete ZIP definition, decompression, or archive rebuilding is included. See the [ZIP specification](https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT). |
| SQLite | Fixed header fields, bounded pages, and data-derived type mappings provide building blocks. | No complete database reader exists; SQLite's variable-length integer encoding is not LEB128, so the current `varuint` reader is unsuitable for its records. See the [SQLite file format](https://www.sqlite.org/fileformat.html). |
| CSV | The CSV reader handles quoted fields and multiline records in requested portions. | The CSV backend links cells and plotted points to exact raw bytes, including quoted and multiline fields. The generic interpreter has no CSV tokenizer; length-changing CSV edits are not implemented. See [RFC 4180](https://www.rfc-editor.org/rfc/rfc4180.html). |

Generic collections with a known fixed item size remain lazy. **Decoded structure** and
**Decoded samples** fetch 128-item pages in background workers. The table defaults
to a repeated numeric collection, preserves definition column order (including
nested fields), and scrolls horizontally through all columns with a pinned header.
Choose another collection explicitly when needed. A sample-row click selects all
its bytes; a value-cell click selects only that field. Selecting Hex bytes locates
the sample's page even when the Decoded structure view is closed.

**Plot** navigates independently of table pages, using sample index on its horizontal
axis. Panning and zooming request the visible range in a background worker. Detailed
views show continuous lines with markers when samples have enough space. The exact
sample budget follows the physical canvas width, bounded at 2,048–8,192 samples per
field. Larger fully read ranges retain each group's first, minimum, maximum, and
last samples in time order, using up to two groups per physical pixel (at most
4,096 groups per field). Original byte locations remain available for selection.
Resolved waveforms stay continuous lines; cycles too dense for the screen become
amplitude bands instead of aliased zigzags. Zoom in to see their individual samples.
This affects rendering only; decoded values, byte edits, and exports stay exact.

**Overview** shows the collection's full range. For large ranges, its initial bands
are estimates from up to 128 distributed, contiguous windows of up to 512 samples
each: at most 65,536 samples in total. These windows reveal local waveform variation,
but peaks in unread regions can be missed. **Scan for all peaks** explicitly reads
every sample in the current range and progressively builds a min/max trace.
Navigation and cancellation stop that scan; opening or plotting a file does not
require an initial whole-file scan. Recent results are cached in bounded memory for
the current source and definition. Plot displays up to 32 numeric fields.

Inside the plot, drag to pan or use the mouse wheel to zoom. Right-drag marks a
horizontal zoom range and zooms to it on release, preserving the vertical scale;
**Escape** cancels the gesture.

Wheel over the bottom **X axis** to zoom X only, or over the left **Y axis** to zoom
Y only. **Shift + wheel** pans the hovered axis; inside the plot it pans X. These
axis gestures work independently, including Y-axis scrolling while **Lock Y** is on.
**Lock Y** is enabled by default and keeps the vertical scale during panning and
zooming inside the plot. Uncheck it to allow vertical adjustment there. **Fit Y**
fits the vertical axis to the currently visible channels.

A plot-point click selects that field's exact bytes. Range selection chooses complete
contiguous samples, including all their fields and padding. **Inspect / edit bytes** opens
Hex at the selection for overwrite or replacement. **Export selected bytes…** streams exactly
the selected bytes, including unsaved edits, into a separate file. A selected sample
range exports the interleaved records; it does not extract one channel into a new
numeric format. Hex and decoded-table selections highlight the corresponding plot
samples and bring them into view when necessary. Undo/Redo updates the same shared
selection, and byte edits invalidate stale plotted values before decoding again.

Fixed-width generic numeric values can be edited directly in Inspector; other
values remain editable through Hex. Variable-size structures require a
boundary scan and are limited to 20,000 decoded nodes. Generic definitions have a
256 KiB YAML limit, 64 nesting levels, a 4,096-field limit for generated types, and a
1 MiB limit per decoded text value. See the reference for detailed rules.

The format-independent model is extensible, but not every format is supported yet.
Built-in byte-backed field types do not include bitfields or UTF-16. Python can
inspect such data using its libraries, but computed/decompressed buffers are not
yet registered as editable source-backed collections. Automatic checksum repair
and length-changing structured export are absent. Editing a count or type can change later decoding;
it does not insert bytes, relocate data, or repair checksums. There are no write
rules that rebuild a container from its edited structure.

## Binary demo: select, inspect, edit, save

1. Choose **File → Open sensor demo**, then **Decode file** (F5). The embedded demo works
   regardless of the working directory. Loading YAML does not execute it; the
   automatic byte preview for external files does not apply the demo layout.
2. X defaults to record index and Y to temperature. Click the obvious spike at
   **record 73**, temperature **85**. Its table row is selected and scrolled into
   view; the hex panel highlights temperature's bytes at **0x380–0x381** (896–897).
   The record begins at **0x37C** (892).
3. Enter **23.45** in the table's selected-cell editor and click **Commit value**
   (or press Enter). This stores integer **2345**, bytes **29 09** in little endian.
   The plot, table, and hex view update. Orange values with `*` mark modified cells;
   orange bytes differ from the original; blue backgrounds mark the selected field.
4. Try **Undo**, then **Redo**. Select another table cell to move the point highlight
   and inspect that field's bytes. Commit or cancel pending cell text before moving
   the selection. Plot dragging pans the view and never edits values.
5. Change X to **timestamp**. Records 70, 72, and 73 share a timestamp, and timestamps
   are unsorted nearby. Clicking the spike still selects record 73. **Connect
   records** is optional and follows source record order. Pan, scroll to zoom,
   right-drag to box zoom, or use **Full range**, jump, and arrow controls.
6. Use **Save edited copy…** and **Save YAML…**. The suggested binary filename is
   different from the source; replacing any existing destination requires explicit
   confirmation. Unsaved edits, YAML text, and pending cell text are guarded when
   replacing the open file or closing the app. Applying a different layout to edited
   bytes requires confirmation: it **keeps the working bytes and shared byte history**.
   Exported copies do not replace the original used for byte highlights.

The matching files are [samples/data/sensors.bin](samples/data/sensors.bin) and
[samples/decoders/yaml/sensors.yaml](samples/decoders/yaml/sensors.yaml). You can also use **Open file…**,
**Open decoder…**, paste YAML, or drop a file into the window. Regenerate the exact
2,424-byte demo (16-byte header, 200 twelve-byte sample records including four
padding bytes each, a four-byte footer marker, and a four-byte checksum field) with:

```sh
python3 samples/generators/generate_binary_demo.py
```

Under **Decoder → Record layout**, the **Section** selector starts on
`samples`. Switch to `header` to inspect its uninterpreted bytes, or `footer` to inspect and edit its ordinary `checksum` field.
The marker is at **0x970** (2416); checksum bytes start at **0x974** (2420).
The checksum value is illustrative: BinWave neither verifies nor recalculates it.
Switching sections resets the plot/table selection; undo/redo follows the edited
field back to its section. The layout summary lists each section's physical range,
record size, payload offset, count, markers, gaps, and trailing byte count. The hex
panel always addresses the whole file, including gaps, markers, and trailing bytes.
Scroll or jump to any position even when there is no numeric selection.

## Legacy section and record YAML

```yaml
endian: little

sections:
  - name: header
    bytes: 16

  - name: samples
    repeat: until next
    fields:
      - timestamp: u32
      - temperature: {type: i16, scale: 0.01}
      - pressure: u16
      - padding: 4

  - name: footer
    marker: "DE AD BE EF"
    fields:
      - checksum: u32
```

The required `endian` is `little` or `big` and applies to all numeric fields.
Sections run in list order from physical file offset zero. Section names must be
nonempty and unique. Each section declares exactly one of `bytes: N` (a nonnegative
uninterpreted byte count) or `fields` (one structured record). Numeric field names
must be nonempty and unique within their section; the same name in different
sections is allowed. An opaque section can be inspected but has no editable values.

Fields are consecutive, with **no implicit alignment**. Both compact fields
(`- timestamp: u32`) and expanded fields work:

```yaml
- temperature:
    type: i16
    scale: 0.01
```

Supported types are `u8`, `i8`, `u16`, `i16`, `u32`, `i32`, `f32`, and `f64`.
`padding: N` is a reserved entry that advances by a nonnegative number of preserved
bytes; multiple padding entries are allowed. Record sizes and field offsets are
calculated automatically. In the example, offsets are 0, 4, and 6, with padding
at 8–11 and a record size of 12. The new format has no field offsets or manual
record sizes. A record must contain at least one byte; a padding-only record is
allowed for inspection. A zero-length opaque region is allowed (for example, a
marker-only section with `bytes: 0`); zero padding is a no-op.

Displayed value = stored value × scale. Scale defaults to one and must be a finite,
nonzero decimal; negative values and scientific notation are supported. Integer
edits divide exact decimal input by the exact YAML scale, rejecting fractional
stored values and type overflow. For example, `23.45` at scale `0.01` is exactly
representable; `23.451` is not. Float edits round to their storage type and report
the actual encoded-and-decoded result: float32 `0.1` at scale `1` shows
`0.10000000149011612`. Existing NaN payloads, infinities, and signed zeros remain
byte-exact unless edited. `NaN`, `inf`, and `-inf` display explicitly; non-finite
X or Y samples are excluded from plots.

### Section boundaries and repetition

| Syntax | Meaning |
| --- | --- |
| No `repeat` | Read one record, or one opaque `bytes` region. |
| `repeat: 100` | Read exactly 100 records; the count must be a positive integer. |
| `repeat: eof` | Read complete records to physical EOF; allowed only on the final section. |
| `repeat: until next` | End at the immediately following section's declared `at` or `marker`. |
| `at: 4096` | Begin at absolute byte offset 4096, preserving a gap if necessary. |
| `marker: "DE AD BE EF"` | Verify these literal bytes at the section start and consume them once. |

`repeat` is allowed only with `fields`. Backward positions and overlapping sections
are errors. Gaps before an `at` offset are uninterpreted and preserved. Bytes left
after all declared sections are also preserved and reported as an uninterpreted
trailing region; `repeat: eof` instead requires an exact number of complete records.
Fixed counts that exceed available bytes fail, without accepting partial records.

A marker belongs to the section declaring it. Its fields or opaque region begin
**after** the marker, and `bytes: N` excludes the marker length. A repeated section
consumes its marker just once, before record zero. Markers contain space-separated,
two-digit hex bytes; they are not integers and their order is independent of
endianness. Empty or malformed markers are errors. An ordinary section verifies
its marker exactly at the current cursor, or its explicit `at`; it never searches
ahead for a match.

`until next` requires the immediately following section to declare `at`, `marker`,
or both. With `at`, repetition stops exactly at that offset, which must align with
a complete record boundary. With both declarations, the next section verifies its
marker there. With only a marker, it is checked **before each record**, including
before record zero; the first match at a record boundary wins. The next section
then consumes the marker. Marker bytes inside or straddling records do not stop
repetition. A missing marker before EOF or a partial final record is an error.
**Marker-only termination is ambiguous if ordinary records can start with the same
bytes.** There are no heuristics to resolve this; use an absolute `at` boundary when
that ambiguity is possible.

YAML is compiled and boundaries are validated against the working bytes before a
layout becomes active. Boundary validation runs in a background task with progress
and cancellation; marker-only repetition requires a scan of record boundaries.
Validation failure leaves the active layout, selection, edits, and history intact.
Errors name sections or fields (or parser paths) and include YAML line/column
locations when available. Unknown keys, duplicate names/keys, unsupported types,
invalid lengths/scales, overflow, conflicting declarations, custom tags, and mixed
schemas are rejected. This section-based reader accepts literal lengths and counts;
use the separate generic `structures`/`actions` interpreter for references and
named calculations. Neither YAML reader executes arbitrary code; Python decoders use the separate Python runtime.

### Existing layouts

The previous `endianness`, `data_offset`, `record_size`, and explicit field `offset`
schema remains supported through a separate legacy parser. Its original type names
(`int8`, `uint8`, `int16`, `uint16`, `int32`, `uint32`, `float32`, `float64`), offset
rules, and finite nonzero scale semantics are unchanged. Legacy data still extends
from `data_offset` (default zero) through EOF and must be an exact multiple of
`record_size`. It is normalized to a single `records` section, with any prefix shown
as an uninterpreted gap. New-format `endian`/`sections` cannot be mixed with legacy
top-level keys; existing files are not silently migrated.

### Compressed sections in YAML

Built-in compressed sections are **not implemented**. A `compression` key explicitly fails
with “compressed sections are not supported yet”; compressed bytes are never
silently treated as ordinary records under that declaration.

The current core separates YAML compilation and physical boundary resolution from
byte access, numeric decoding, sparse editing, and export. A future compressed
section will distinguish (1) its physical byte range in the original file,
(2) a separately owned decoded buffer produced by decompression, and (3) field
offsets within that buffer. Decompressed fields generally have no one-to-one mapping
to original compressed bytes; inspection may need a separate decoded hex view.
Compressed sections should initially be read-only. Editing/saving them would require
explicit support for recompression, changed lengths, relocated sections, and affected
checksums. No decompressor, recompressor, or plugin framework is included now.

## Legacy binary storage and limits

- Raw opening, raw edits, and generic definitions use the shared original-file byte
  view described above. Applying a **legacy section/record layout** prepares a
  private cached snapshot for its numeric editor,
  memory-mapped read-only. Unchanged files reuse their snapshot across application
  restarts. On Linux,
  the initial snapshot uses a filesystem copy-on-write clone when supported;
  otherwise it copies with progress and cancellation. Only changed bytes and
  edit history are held separately. Headers, markers, padding, gaps, trailing bytes,
  and other unmodified bytes never pass through numeric re-encoding. An
  unedited export is byte-identical; undoing every edit restores the original bytes.
- There is **no fixed MiB/GiB cap**. The platform mapping limit is `isize::MAX`
  bytes (about 8 EiB on 64-bit, 2 GiB on 32-bit), further constrained by address
  space, filesystem limits, and free cache disk space. Without filesystem cloning,
  a binary snapshot needs space equal to the source file. Set `BINWAVE_CACHE_DIR`
  to a suitable local disk if needed. During replacement the previous snapshot remains until the new one is
  ready. Snapshot/mapping failures leave the existing document intact and show
  an error. Stop external writers while taking the initial snapshot.
- Export streams through a 1 MiB buffer to a temporary file in the destination
  directory, flushes it, then publishes it atomically. That directory needs space
  for the full output, including when replacing a file. Failed or unconfirmed
  saves do not truncate an existing destination.
- At most **2^53 records**, so record-index X values remain exact in the plotting
  library's f64 coordinates. Every rendered sample separately retains its original
  integer record index and absolute byte offset, including after downsampling.
  Binary series also retain their section and X/Y fields; selections and history
  carry section, local record, field, absolute byte offset, and byte length.
  Records are never located by plotted X values alone.
- Binary plotting builds a min/max hierarchy once per section and X/Y pair in a
  background worker, retaining up to **4 cached pairs** and **8 views per pair**. An index has
  at most **32,768 source-record blocks**; blocks grow with file size to bound index
  memory (roughly 7 MiB per pair). Covered blocks use cached extrema; only partially
  visible blocks need decoding again. Edits, undo, and redo rebuild only affected
  blocks and invalidate affected view caches. Applying a layout invalidates indexes.
  Binary indexes are kept in memory for the current document; a first plot for a
  new pair still needs an initial scan.
- Plots retain min/max Y within visible source-record groups, keeping original
  sample identities and at most **4,096 samples** in the binary view. With at most
  4,096 visible records every sample is shown. The selected sample is highlighted
  separately. Duplicate/unsorted X values remain supported. A new narrow view of
  heavily unsorted X can still require a broad scan because each source block may
  span the viewport; repeated views reuse cached results. Indexing/scanning displays
  “Updating plot”. Edit overlays and history grow with the number of edits.
- The legacy table pages **100 records** at a time. Hex continuously scrolls the
  shared whole-file byte view and reveals the selected field. Legacy section/record
  YAML is limited to **64 KiB, 256 sections, and 256 numeric fields total** to bound
  parsing and table work. Decimal inputs support
  up to **1,024 characters**, with an explicit exponent between −1,024 and 1,024;
  scales must also fit a finite nonzero f64 for plotting.
- CSV plots and Markdown configuration use the original-file access described
  below, with bounded initial reads and optional whole-file indexing.

The core in `src/binary/` separates schema validation, numeric encoding/decoding,
physical section resolution, snapshot/edit history/export, and sample selection
from `src/binary_ui.rs`. The shared overlay and export live in `src/byte_store.rs`;
`src/hex_editor.rs` owns the byte surface, and `src/structure.rs`,
`src/structure_ui.rs`, and `src/structure_views.rs` implement generic interpretation
and its workspace views.

## CSV workflow and Markdown configuration

Choose **Open file…** or drop a CSV. **Decoder** selects **CSV** and opens the
first portion of the file; **Hex** can already browse the whole file. For delimited
text with another extension, select **CSV** in Decoder and press **Apply**. Python and YAML remain
available in the same selector, with their drafts preserved.

Choose the **Separator**, whether the **First row is a header**, and the **X column**,
then press **Apply** (F5). For a headerless file, turn off the header checkbox and
use `c0` for X; columns are named `c0`, `c1`, etc., and row zero is retained.
Settings changes take effect when applied. Existing results remain visible until
the replacement rows arrive. **Open configuration…** loads a Markdown configuration
and its data file; the same command remains in the File menu.

A cold open requests **up to 8,192 rows**, stopping sooner if it reaches the
interactive byte budget. Each request indexes up to **8 MiB of new input**, plus
the final record (at most **1 MiB**) and **64 KiB** of read-ahead. Header bytes count
toward that budget and have the same 1 MiB record limit. No automatic scan continues
to the end of the file after that request finishes. An existing cached index may
make more rows available immediately.

- In **Decoded samples**, use **Next rows**, change the page, or **Go to row**
  (numbered from zero). A request beyond the indexed portion reads forward in
  bounded batches until the requested page is found or EOF is reached. A distant
  row can take time because CSV records have variable lengths and quoted newlines.
- In **Plot**, **Next** reads another portion when you reach the available range.
  **Previous**, **Start**, and zoom controls navigate the known data. **Columns**
  holds search, individual visibility checkboxes, **Show all**, and **Hide all**;
  these visibility choices also determine the table columns. **Available range**
  fits the rows known so far; it becomes **Overview** when the file is complete.
- **Index entire file**, available in Decoder and Plot, explicitly scans every
  remaining row for an exact row count, complete X range, and whole-file summaries.
  **Stop** cancels the active request while keeping rows already available. Later
  errors also leave the accepted portion usable with an error message.

Click a table value or a plotted sample to select its raw field bytes in **Hex**.
Click a row number for the entire record. Selecting bytes in Hex reveals the
corresponding table row and marks the sample in Plot; headers and blank lines
have no sample. The plot preserves its zoom and pans only when the selected X
is outside the current view. Byte selections remain exact, including raw quotes
and escapes; an empty CSV cell selects no bytes. Use Hex's selection operations
to copy, export, or overwrite the selected bytes.

Selecting bytes beyond the indexed prefix requests bounded batches until that
record is found. This can take time for a distant offset; **Stop** cancels it.
Plot selection retains the actual source row, including duplicate or unsorted X
values. Changing the Python/YAML/CSV editor leaves these results and links intact.

The status reports rows **available** until EOF is known, then reports the file as
**complete**. An idle, partly indexed file is ready to use. Completed portions are
saved as resumable indexes: reopening an unchanged file restores those rows, and
the next request resumes from the saved record boundary. Opening another file
clears the previous file's CSV views.

Examples: `samples/data/sample.csv` and `samples/configs/sample_config.md`.
A Markdown configuration contains a fenced JSON block:

````markdown
# BinWave Config

```json
{
  "data_path": "samples/data/sample.csv",
  "csv": {
    "delimiter": ",",
    "has_header": true,
    "time_column": "time",
    "ram_cache_mib": 64
  },
  "view": { "max_lines_in_plot": 2000, "target_points_per_line": 2000 }
}
```
````

The configured data path is relative to the application's working directory.
`csv.include_signals` optionally limits the columns decoded; hiding a column in
Plot only changes its visibility. Generate an optional large CSV with:

```sh
python3 samples/generators/generate_large_csv.py --gb 20 --signals 8 --out samples/data/large_test.csv
```

## CSV storage and large-file reading

**CSV values stay in the original file.** BinWave keeps a compact index of row
positions and plot summaries for the portion it has read. Neither normal browsing
nor **Index entire file** creates a full numeric copy.

- **Decoded RAM cache: 64 MiB by default**, shared across signals and views.
  Older blocks are evicted and parsed again when needed. `csv.ram_cache_mib` in a
  configuration sets this budget (1–4,096 MiB, subject to platform limits).
  This is a decoded-block budget, not total process memory: indexes, parser buffers,
  the current block, plot points, UI state, and the OS file cache are additional.
- CSV records, including quoted multiline records and headers, are limited to
  **1 MiB of source bytes each**. Oversized records fail with an explicit error;
  earlier accepted rows remain available.
- Seek checkpoints are capped at **65,536 entries**, and plot summaries have a
  bounded block count across signals. A saved index is limited to **64 MiB**.
  Both partial and complete indexes are reused when the source and parsing settings
  match. Invalid indexes are rebuilt. If index storage is unavailable, the open
  file stays usable with its index in memory.
- Known-range plots use stored extrema. A narrow zoom may read a block again after
  eviction; unsorted X values can require wider reads. Table and plot requests run
  in background workers, and four rendered views are cached per signal.
- Keep the original CSV available and unchanged. Source identity is checked before
  queries and after reads. After a detected change, reopen the file or reapply its
  CSV settings to rebuild the index. This is not a live-tail viewer.

Older configurations may contain `csv.decoded_disk_cache` and
`csv.decoded_cache_limit_mib`. They remain accepted for compatibility, but the
interactive workspace always uses the original-file reader and ignores those
decoded-copy settings.

### Cache location and clearing

The directory is `binwave-v1` beneath `BINWAVE_CACHE_DIR`, otherwise
`$XDG_CACHE_HOME`, `$LOCALAPPDATA`, or `~/.cache` (in that order; the temporary
directory is the final fallback). Processing and caches remain local. Keys include
canonical source path, size, modification time, Unix identity/change time when
available, format version, and CSV parsing settings. Changing storage or view
preferences does not invalidate a compatible index.

**Clear disk cache** removes disposable entries while retaining open data, original
source files, edited-copy exports, and unsaved edits. Some operating systems require
closing mapped files before removal. Advancing a CSV prefix replaces its old
index entry; indexes for different files/settings and binary snapshots can still
accumulate.

Decoded copies created by earlier versions remain on disk until cleared. The
interactive CSV reader does not use them. **Clear disk cache** also reclaims that
older storage.

### Reproducible measurements

Run the original-file/index **full-scan** benchmark with:

```sh
cargo test --release benchmark_csv_source_index -- --ignored --nocapture
```

It generates one million rows and eight signals, measures first usable rows, the
full index scan, index size, reopening, and an uncached narrow zoom. It measures
explicit indexing, not the bounded default open. To use an existing
**headerless** CSV, set `BINWAVE_BENCH_CSV` to its path and
`BINWAVE_BENCH_CACHE_PARENT` to an existing scratch directory. The benchmark
creates and removes its own temporary index; it never changes the source.

A local one-million-row run used a 663,067-byte index for 42,920,530 bytes of CSV.
First usable rows arrived in 2.1 ms, indexing took 204 ms, reopening took 2.5 ms,
and an uncached narrow zoom took 1.3 ms. These measure loader/query work, not native
window painting, and do not flush the operating system's file cache. Timings depend
on hardware, filesystem cache state, and data.

For the existing 20.0 GiB `large_test.csv` (439,600,000 rows, eight signals),
the same full-scan benchmark produced a **756,954-byte index (739 KiB)** and peaked
at about **71 MiB process RSS**. First 8,192 rows arrived in **2.2 ms**; the full
scan took **85.9 seconds**, reopening took **2.0 ms**, and a previously uncached
narrow zoom took **351 ms**. No full decoded copy was created. These measurements
exclude GUI memory and OS file-cache memory. They predate the bounded-opening
workflow and describe a complete scan; they do not imply that opening now scans
the whole file.

The older `benchmark_csv_cache` and `benchmark_csv_first_rows` benchmarks explicitly
exercise the legacy full decoded backend without its policy limit. They can
create large temporary copies and do not represent the interactive CSV workflow.

## Verification

```sh
cargo fmt --check
cargo test
cargo build --release
```

Tests cover both byte orders and all numeric types, exact integer scaling and
bounds, float32 rounding, non-finite bytes, section identities, compact/expanded
field syntax, calculated offsets, all repeat rules, absolute boundaries and gaps,
marker ownership and record-boundary matching, missing markers, partial records,
trailing regions, legacy compatibility, mixed/invalid schemas, compression rejection,
header/marker/padding/gap/footer preservation, exact
unedited export, overwrite protection, undo/redo and saved checkpoints, invalid
layouts, incomplete records, and duplicate/unsorted X selection with extrema
preservation. Cache tests cover cold/warm opens, source/settings invalidation,
immutable live mappings, headerless CSV row zero, early header failure, cancellation,
summary queries without source reads, and updating only the edited index block.
CSV tests cover bounded initial requests, forward row requests, resumable partial
indexes, explicit full indexing, source/settings invalidation, oversized records,
and usable prefixes after cancellation or errors. Storage tests verify that only
an index is written, quoted/multiline records seek correctly after eviction,
decoded blocks stay within their RAM budget, and unavailable index storage leaves
the file usable. Legacy decoded-cache tests retain compatibility coverage.
Background plot tests cover cancellation and accepting useful results while more
rows arrive. Egui tests exercise column controls and navigation during reading.
An egui event test opens the demo, clicks Apply and the spike, types
a table edit, undoes/redoes it, and verifies the asynchronously exported bytes.
Another egui test switches between sample/footer/header sections, selects footer
bytes from the plot, undoes across sections, checks failed-Apply preservation, and
inspects opaque hex data. These tests bypass the OS file picker; they are not native
desktop automation tests. File-dialog regressions simulate a blocked native backend
while rendering egui frames, then verify cancellation, completion, duplicate-dialog
prevention, and preservation of the bytes or definition captured for saving.

Workspace tests cover the Decoder-and-Hex default, reopening and persisting panes,
full-height virtualized hex rendering, mouse-to-keyboard overwrite focus, range
selection, large-offset navigation, chunk-boundary searches, and source-file alias
protection. Generic interpreter tests cover nested description-derived types,
references, checked arithmetic, bounded regions, alignment, lazy large collections,
PNG/RIFF examples, and builder YAML round trips. Standalone Python SDK tests run with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s python -p 'test_*.py'
```

They cover the subprocess protocol, imports and console output, tracebacks,
bounded reads, lazy collections, header-derived types, and reordered descriptor-driven data.


## Large waveform parser fixture

The [40 GB waveform test guide](samples/waveform_stress_guide.md) covers the generated
`waveform_stress_40gb.bin`, its small previews, the
[Python decoder](samples/decoders/python/waveform.py), and an equivalent
[YAML definition](samples/decoders/yaml/waveform.yaml). Twelve distinct waveform channels
are discovered from embedded descriptors; reusable section headers and their lengths
locate the data. A reordered, relocated preview uses the same script or YAML.

The [standalone generator](samples/generators/generate_waveform_stress.rs) writes all 40 GB with a
32 MiB buffer; large generated files are ignored by Git. This is a documented
synthetic tagged container identified by `PWAV`, generated entirely from mathematical waveform formulas.
