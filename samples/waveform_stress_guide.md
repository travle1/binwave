# Waveform parser stress fixture

PWAV is a synthetic tagged container for testing Binwave's reusable decoders.
Its numeric samples are generated from mathematical waveform formulas. The file
carries names, types, widths, counts, and section lengths so the decoder discovers
the sample layout from metadata.

For a ready-to-present CSV and binary pair, see the [2 GB demo guide](waveform_demo_guide.md).

## Generate a small example first

From the project directory:

```bash
rustc --edition=2021 -O samples/generators/generate_waveform_stress.rs -o /tmp/generate_waveform_stress
/tmp/generate_waveform_stress --output samples/data/waveform_preview.bin --size-bytes 1048576
```

This creates 16,319 records in a 1 MiB file. Existing destinations are never
overwritten. Generate another version with the same records, reversed descriptor
and field order, and a larger unknown section before the sample payload:

```bash
/tmp/generate_waveform_stress --output samples/data/waveform_reordered.bin --size-bytes 1052672 --reordered
```

The second file moves the sample payload from byte 4096 to byte 8192. Both decoders
below must discover that change without editing their source.

## Open and decode

1. Open the generated `.bin` file. Hex is available immediately.
2. In **Decoder**, choose **Open decoder…** and open
   [waveform.py](decoders/python/waveform.py). Opening shows the Python source;
   choose **Run script** (F5) to execute it. Python 3 must be installed.
3. The console reports discovered fields and registered sample count. Choose
   **Show samples** or **Show plot**, and select `samples` if necessary.
4. Zoom to samples 0–127 to compare waveform shapes. The table pages through 128
   records at a time; plotting navigates independently across the collection.
5. Select a plotted value and change **Value** in Inspector, or choose
   **Inspect / edit bytes** to work in Hex. Select complete samples and use
   **Export selected bytes…** to extract their interleaved bytes, including edits.

An equivalent [YAML decoder](decoders/yaml/waveform.yaml) is available. Open it,
choose **Decode file** (F5), and select `sections[2].payload.records`. The
**Python / YAML** selector preserves separate source drafts; it does not convert
between languages. **Run** executes the active source explicitly.

**View → Decoder workspace** restores the editor beside Hex. **View → Plot and
bytes workspace** shows Plot and Hex together. See the
[Python API guide](../python/README.md) for interpreter setup and byte access.

## What decoding demonstrates

The Python script validates the header and walks section IDs and lengths using a
normal loop. Descriptors define the `Sample` structure from encoded field names,
numeric format codes, and byte widths. The sample section registers
`file.records("samples", Sample, count=header.sample_count)` without scanning every
value. Seeking past its declared length finds the footer. Unknown sections remain
inspectable as raw bytes.

The YAML definition expresses the same sequence with reusable structures,
length-bounded sections, type selection by tag, and a dynamically defined sample
type. Neither decoder contains absolute sample offsets or copied channel names.
Reordering descriptors and widening the unknown section changes the file layout
without changing the decoder.

Results remain typed byte ranges over the open file. Plot points, sample cells,
Hex selections, and edits share exact addresses. Python reads the working byte
revision, including unsaved Hex edits; byte changes invalidate results and rerun
the last applied script. Source changes require **Run script**. Python uses local
user permissions and imported libraries; it is not sandboxed. Console output and
errors arrive when execution completes.

## Plot navigation

Detailed ranges show continuous sample lines and markers. Denser, fully read
ranges retain first/minimum/maximum/last samples per group as a continuous trace.
**Overview** initially estimates
amplitude bands from at most 128 contiguous windows of up to 512 records each,
for at most 65,536 records. Peaks in unread regions can be missed. **Scan for all
peaks** reads every sample in the current range and builds a min/max trace;
the scan is cancellable and does not load the whole file into RAM.

Drag to pan and use the wheel to zoom. Right-drag a horizontal range to zoom to
it; **Escape** cancels the gesture. Wheel over the X or Y axis to zoom that axis
only, and **Shift + wheel** pans the hovered axis. **Lock Y** keeps the vertical
scale during navigation inside the plot; direct Y-axis scrolling still adjusts it.
**Fit Y** fits visible channels. Table paging remains independent of plot
navigation, while selections are shared with Hex.

## Container recipe

All integers and floats use little-endian encoding. The 32-byte header is:

| Field | Type |
|---|---|
| signature | Four text bytes: `PWAV` |
| version | u32, value 1 |
| file_bytes | u64 |
| sample_count | u64 |
| section_count | u32 |
| descriptor_count | u32 |

Each section starts with a u32 ID, u32 flags, and u64 payload length. ID 1 holds
24-byte descriptors: a 16-byte zero-padded name, u32 numeric format code, and u32
byte width. Codes 1–6 represent u64, i64, f64, f32, u32, and i32. ID 2 holds sample
records defined by those descriptors. ID 3 holds the 48-byte footer: `PWAV-END`
followed by five u64 values for sample count, sample payload bytes, sample offset,
final index, and generator seed. Other IDs are length-delimited opaque payloads.

The generator emits four sections: descriptors, an unknown section, samples,
and footer. The unknown section makes the sample offset vary between recipes.
These tags and layouts belong to this example; the application has no built-in
PWAV parser or format-code table.

## Waveforms

Each sample has 12 fields and consumes 64 bytes. Their vertical offsets separate
the patterns when plotted together; the values are illustrative measurements.
All patterns repeat as a group every 384 samples.

| Channel | Type | Pattern | Range / band | Period |
|---|---|---|---|---:|
| sine | f64 | Sine | -4 to 4 | 64 |
| triangle | f64 | Triangle | 6 to 14 | 96 |
| ramp | f64 | Rising sawtooth | 16 to 24 | 128 |
| chirp | f64 | Increasing sine frequency | around 30 ± 4 | 128 |
| square | f32 | High/low phases | 37 or 43 | 64 |
| pulse | f32 | Eight-sample pulse | 50 or 56 | 96 |
| staircase | f32 | Eight rising steps | 60 to 67 | 128 |
| damped_sine | f32 | Decaying sine burst | around 70 | 128 |
| zigzag | i32 | Quantized triangle | 80 to 88 | 32 |
| alternating | i32 | Alternating phases | 97 or 103 | 16 |
| counter_ramp | u32 | Wrapping integer ramp | 110 to 125 | 16 |
| bit_pattern | u32 | Four-bit Gray-code sequence | 130 to 145 | 16 |

The [generator](generators/generate_waveform_stress.rs) contains the exact formulas.
It seeds a bounded buffer with one period, then copies repetitions and writes
every output byte; its large output is not sparse.

## Optional 40 GB generation

```bash
/tmp/generate_waveform_stress --output samples/data/waveform_stress_40gb.bin --size-gb 40
```

The standard layout produces exactly 40,000,000,000 bytes, with 624,999,935 records,
a 4096-byte header area, and a 64-byte final section. On Linux, the optional
`--no-compression` flag applies `chattr +m` to the new file so compression does not
reduce the physical size of this repetitive dataset. The generator uses a
32 MiB write buffer and refuses to replace an existing destination.

Verify generated metadata and distributed sample probes:

```bash
/tmp/generate_waveform_stress --verify samples/data/waveform_preview.bin
```

Verification recognizes both generated field orders. It checks header/footer and
sampled records; it does not compute a full-file checksum.

## Automated checks

These tests create their own fixtures and run without generated sample files:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s python -p 'test_*.py'
cargo test --offline --test waveform_stress
```

They cover descriptor reordering, shifted sample offsets, descriptor-driven type
changes, malformed lengths, and lazy access beyond 4 GiB. The large address test
uses a temporary sparse file and writes only its checked records.

For an existing generated fixture, opt into the production-runtime checks:

```bash
BINWAVE_WAVEFORM_FILE=samples/data/waveform_preview.bin cargo test --offline --bin binwave verify_generated_waveform_python_runtime -- --ignored --nocapture
BINWAVE_WAVEFORM_FILE=samples/data/waveform_preview.bin cargo test --offline --test waveform_stress verify_generated_waveform_stress_file -- --ignored --nocapture
BINWAVE_WAVEFORM_FILE=samples/data/waveform_preview.bin cargo test --offline --test waveform_stress verify_generated_waveform_plot_ranges -- --ignored --nocapture
```

Without the environment variable these optional tests use
`samples/data/waveform_stress_40gb.bin`. Their checks read bounded metadata and
sample ranges, including the beginning, middle, and end; they do not claim to
verify every byte of the large file.
