# Binwave Config (Markdown)

Open with **File → Load CSV configuration…**. The sample data path is relative
to the process working directory; launch Binwave from the project root.

```json
{
  "data_path": "samples/data/sample.csv",
  "format": "csv",
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
    "max_lines_in_plot": 2000,
    "target_points_per_line": 2000
  }
}
```
