# 2 GB waveform demos

Two fully populated files contain the same 12 waveform channels: sine, triangle,
ramp, chirp, square, pulse, staircase, damped sine, zigzag, alternating, counter
ramp, and a Gray-code bit pattern. These are synthetic public demo data.

| Data file | Size | Decoder or configuration |
| --- | --- | --- |
| [waveform_demo_2gb.csv](data/waveform_demo_2gb.csv) | 2,000,000,000 bytes | [CSV configuration](configs/waveform_demo_2gb.md) |
| [waveform_demo_2gb.bin](data/waveform_demo_2gb.bin) | 2,000,000,000 bytes | [YAML](decoders/yaml/waveform.yaml) or [Python](decoders/python/waveform.py) |

The binary holds **31,249,935 samples**; the CSV holds **17,047,452 samples**.
CSV uses more bytes per sample and therefore holds fewer at the same file size.
Channel values at matching sample indices are identical, including f32 rounding.
The CSV `time` column is a sample index, not elapsed seconds. A 56-byte blank-line
tail fills the CSV's final bytes; there are no truncated records.

## Open the CSV demo

1. Choose **File → Open file…** and open `samples/data/waveform_demo_2gb.csv`.
   The header and first samples load automatically, with numeric channels plotted.
2. For explicit settings, choose **File → Load CSV configuration…** and open
   `samples/configs/waveform_demo_2gb.md`. Launch the app from the project root
   so the relative data path resolves.
3. View samples 0–383 to see a complete repeat of every waveform. Use **Columns**
   in Plot to show a smaller set of channels. Select a point or table cell to
   highlight the corresponding text bytes in Hex.

The CSV reader opens a bounded portion first. Navigation indexes more rows as
needed; **Index entire file** is optional. The configuration uses a 64 MiB decoded
RAM cache and leaves decoded disk caching off.

## Open the binary demo

1. Open `samples/data/waveform_demo_2gb.bin`. The initial plot is a raw byte preview.
2. Choose **Edit decoder → Open decoder…** and open
   `samples/decoders/yaml/waveform.yaml`, then **Decode file** (F5).
3. Choose **Show plot** and select `sections[2].payload.records`. View samples
   0–383 to compare with CSV; the sample table and Hex share selections.

Alternatively, open `samples/decoders/python/waveform.py` and choose **Run script**
(F5); its sample collection is named `samples`. Both decoders discover field names,
types, sample count, and section lengths from the file. Neither requires a known
sample offset. They register the complete sample collection without reading all
sample values. The [container guide](waveform_stress_guide.md) explains the format
and includes a reordered fixture for testing layout discovery.

## Regenerate and verify

From the project root, compile only the standalone generator:

```bash
rustc --edition=2021 -O samples/generators/generate_waveform_stress.rs -o /tmp/generate_waveform_demo
/tmp/generate_waveform_demo --output samples/data/waveform_demo_2gb.bin --size-gb 2
/tmp/generate_waveform_demo --csv --output samples/data/waveform_demo_2gb.csv --size-gb 2
python3 samples/generators/verify_waveform_demo.py --binary samples/data/waveform_demo_2gb.bin --csv samples/data/waveform_demo_2gb.csv --expected-size-bytes 2000000000
```

Generation uses a 32 MiB write buffer and refuses to overwrite existing files;
choose new output names when keeping previous fixtures. Both encodings use the
same waveform implementation. Verification checks metadata and distributed
samples, not every byte. Large generated files stay local and are ignored by Git;
the generator, decoders, CSV configuration, and this guide can be shared.
