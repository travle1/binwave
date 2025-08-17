Plotter Max
===========

A Rust eframe/egui application for plotting large datasets (CSV and custom binary) with thousands of signals using on-the-fly decimation and a JSON config.

Quick Start
-
- Build: `cargo run --release`
- Load: Click "Open CSV..." or "Open Config JSON..." in the left panel.
- Sample files: `samples/sample_config.json` and `samples/sample.csv`.

Config (JSON)
-
- `data_path`: path to the data file.
- `format`: `"csv"` or `"binary_fixed"`.
- `csv.delimiter`: delimiter character, default `,`.
- `csv.has_header`: whether the CSV has a header row.
- `csv.time_column`: name of the time column.
- `csv.include_signals`: optional list of column names to include.
- `binary`: describe fixed-record binary files.
- `binary.endianness`: `"little"` or `"big"`.
- `binary.record_size`: optional total bytes per record (auto if omitted).
- `binary.fields`: list of `{ name, ty, offset? }` where `ty` is one of `f32,f64,i8,u8,i16,u16,i32,u32,i64,u64`. If `offset` is omitted, fields are tightly packed in order.
- `binary.time_field`: name of field representing time.
- `binary.include_signals`: optional list of field names to include as signals (all non-time fields if omitted).
- `view.max_lines_in_plot`: soft limit for lines.
- `view.target_points_per_line`: target plotted points per line (decimation).

Examples
-
CSV example:

```
{
  "data_path": "samples/sample.csv",
  "format": "csv",
  "csv": { "delimiter": ",", "has_header": true, "time_column": "time" },
  "view": { "max_lines_in_plot": 2000, "target_points_per_line": 2000 }
}
```

Binary fixed-record example:

```
{
  "data_path": "capture.bin",
  "format": "binary_fixed",
  "binary": {
    "endianness": "little",
    "record_size": 24,
    "fields": [
      { "name": "time",     "ty": "f64", "offset": 0 },
      { "name": "ch0",      "ty": "f32", "offset": 8 },
      { "name": "ch1",      "ty": "f32", "offset": 12 },
      { "name": "status",   "ty": "u32", "offset": 16 },
      { "name": "padding",  "ty": "u32", "offset": 20 }
    ],
    "time_field": "time",
    "include_signals": ["ch0", "ch1"]
  },
  "view": { "max_lines_in_plot": 2000, "target_points_per_line": 2000 }
}
```

Notes
-
- Rendering uses `egui::plot::Plot` with min/max bucket decimation per view.
- Loading runs in a background thread; UI remains responsive.
- The architecture leaves room to add streaming sources, ring buffers, and more file formats.
