# Python decoders

Open a data file in BinWave, then use **Decoder → Open decoder…** to load a
`.py` file. Loading only displays its source. **Run script** (F5) executes it;
**Stop** cancels it. **Save Python…** saves the source. To start from scratch,
select **Python** and choose **New**. YAML is the default editor; each language has
its own editable, commented starter. **View → Decoder workspace** restores the editor
beside Hex.

When a nonempty binary file opens, untouched starters become executable byte
preview decoders. The initial plot comes from the built-in native YAML reader,
so Python is not required for opening or previewing a file. Switch to Python to
edit the equivalent `u8` record definition, then use **Run script** to apply it.
Existing loaded, edited, or deliberately cleared drafts are preserved. Only the
explicit Run action executes your Python; subsequent byte edits rerun the last
decoder that successfully published results.

The **YAML / Python** selector chooses between two independent drafts. Switching
keeps both sources; it does not convert or run either one. **Open decoder…** chooses
the language from `.py`, `.yaml`, or `.yml`. Run the active Python draft with
**Run script**, or the active YAML draft with **Decode file**. YAML keeps its
Fields and Steps forms. Select **YAML** and choose **New** for a fresh YAML source.

The runtime uses installed `python3`, or `python` on Windows. Set
`BINWAVE_PYTHON` to a specific executable to select a virtual environment:

```sh
BINWAVE_PYTHON=/path/to/.venv/bin/python target/release/binwave
```

Ordinary imports, functions, loops, and libraries work. The working directory and
relative imports use the saved script's folder. Output and tracebacks appear in
the console when execution finishes; they are not streamed while the script runs.
Python runs with your local user permissions and is **not sandboxed**.

BinWave supplies `file` and the `binwave` module during execution. They do
not require a pip package; run these scripts inside BinWave.

## Run ordinary Python

```python
test = 5
print(test)
```

**Run script** (F5) prints `5` in Console. An assignment on its own is also valid;
it finishes with no printed output. Imports, calculations, and raw byte reads do
not have to create a dataset. **New** starts with a commented example that reads a
header count and describes the following samples. Adapt it to your binary file,
select all, and press Ctrl+/ (Cmd+/ on macOS) to uncomment. This shortcut also
toggles comments on selected lines. Untouched starters do not run or count as
unsaved edits; loading a decoder replaces the starter with its source.

Each Run executes the whole script in a fresh Python interpreter. Variables do not
persist between runs. Use `print()` to display a value; the editor runs scripts
rather than evaluating expressions as an interactive Python prompt.

Runs that publish no data, fail, or are stopped keep the previously decoded results
for the same working-byte revision. Errors appear beside the source in Console.
Only a successful run with registered data replaces Plot, Table, and decoded
structure. Changing the bytes still invalidates results until they are refreshed.

## Read metadata, describe results

```python
from binwave import Field, Struct

Header = Struct("Header", [
    Field("magic", "text", length=4, expect="DATA"),
    ("count", "u32"),
    ("offset", "u64"),
], endian="little")
Sample = Struct("Sample", [("value", "f32"), ("flags", "u32")])

header = file.read_struct("header", Header)
samples = file.records("samples", Sample, count=header.count, offset=header.offset)
print(f"Registered {len(samples):,} samples")
print(samples[0].value if samples else "No samples")
```

`read_struct()` returns a dictionary with attribute access, such as `header.count`
or `header["count"]`. Use these values in Python calculations and conditions to
build further structures or locate later data. The
[Waveform example](../samples/decoders/python/waveform.py) creates fields from embedded
descriptors and walks sections by their IDs and lengths.

`read_struct()` and `records()` register typed ranges for Decoded structure,
Decoded samples, Plot, and Hex. `records()` does not read every sample: the app
loads displayed values on demand. Python emits an internal read plan for the same
byte-backed results engine that YAML uses. YAML is a declarative schema with
reading operations; Python runs ordinary code to construct that plan. The source
formats are not automatically translated into one another.

The current output API represents fixed-size structures over file bytes. Arbitrary
computed arrays, decompressed buffers, and Python callbacks executed for every plotted sample are not registered output types.
You can still use Python libraries to inspect or calculate such values and print them.

Each run reads a consistent version of the working bytes, including unsaved Hex
edits. Byte edits invalidate results and rerun the last decoder that successfully
published data. Plain scripts and failed attempts do not replace that decoder. Changes
to the script editor itself remain a draft until **Run script**. Scripts may
therefore execute more than once while editing a file.

## File access

| Member | Behavior |
| --- | --- |
| `file.size` | File length in bytes. |
| `file.tell()` | Current byte position; initially zero. |
| `file.seek(offset, whence=0)` | Move from file start (`0`), current position (`1`), or file end (`2`). |
| `file.read(size)` | Read bytes and advance; at most 1 MiB per call. |
| `file.read_at(offset, size)` | Read bytes without moving the cursor; same size limit. |
| `file.unpack(format, offset=None)` | Python `struct.unpack` tuple; advances unless an explicit offset is supplied. |
| `file.read_struct(name, struct, offset=None)` | Decode metadata and register a named result; advance past it. |
| `file.records(name, struct, count=None, offset=None)` | Register a lazy collection and advance past its complete range. |

With no offset, structured reads start at the cursor. With no count, `records()`
uses the remaining file and rejects incomplete trailing records. For a bounded
section followed by a footer, pass its count explicitly. Reads and seeks reject
positions outside the file. Raw reads and `unpack()` do not automatically create
displayed results.

Collections support `len(samples)`, `samples[17]`, `samples[-1]`, and slices of at
most 512 items. Indexing leaves the file cursor unchanged. Collections larger than
4,096 items reject implicit iteration or `list(samples)` to prevent accidental
whole-file allocation; inspect indexed values or bounded slices instead.

## Reusable fields

```python
Field(name, type, length=None, count=None, offset=None, endian=None, expect=None)
Struct(name, fields, endian=None)
```

- Types are `u8`, `u16`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`,
  `text`, `bytes`, or another `Struct`. `(name, type)` is shorthand for `Field`.
- `text` and `bytes` require a byte `length`. Text uses UTF-8 and preserves NULs;
  use `.rstrip("\0")` for a padded descriptor name. `length` can also reserve
  padding after a numeric value or nested structure.
- Fields are consecutive with no implicit alignment. `offset` inserts a gap from
  the start of the containing structure; overlapping fields are rejected. Use a
  separate `read_struct` to inspect an overlapping interpretation. A named
  `Field("padding", "bytes", length=4)` also describes padding directly.
- `count` repeats a field. `endian` is `little` or `big`, inherited from its
  structure unless overridden; the default is little endian.
- `expect` checks a numeric or text value. Use a Python assertion for raw bytes.
- Names must be ASCII Python identifiers up to 128 characters. Emitted read names
  are unique. A structure name may be reused with the same definition.

Source and emitted decoder plans are limited to 256 KiB. Console output and
tracebacks are capped at 64 KiB. The bridge bounds each byte read to 1 MiB and uses
a small read cache; these bounds do not restrict allocations or file access made
by arbitrary Python code or imported libraries.

SDK regression tests:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s python -p 'test_*.py'
```
