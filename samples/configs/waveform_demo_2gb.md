# Waveform CSV demo configuration

Open with **File → Load CSV configuration…** from the project root.
This loads `samples/data/waveform_demo_2gb.csv` with all 12 waveform channels.
The `time` column is the zero-based sample index, matching the binary demo's X axis.

```json
{
  "data_path": "samples/data/waveform_demo_2gb.csv",
  "csv": {
    "delimiter": ",",
    "has_header": true,
    "time_column": "time",
    "include_signals": null,
    "ram_cache_mib": 64,
    "decoded_disk_cache": false,
    "decoded_cache_limit_mib": 1024
  },
  "view": {
    "max_lines_in_plot": 12,
    "target_points_per_line": 2000
  }
}
```
