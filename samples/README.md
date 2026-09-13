# Sample files

| Folder | Contents | Open with |
| --- | --- | --- |
| [data/](data/) | CSV and binary data, including generated large fixtures | **File → Open file…** |
| [decoders/python/](decoders/python/) | Python decoder scripts | Select **Python**, then **Open decoder…** |
| [decoders/yaml/](decoders/yaml/) | YAML decoders and structure examples | Select **YAML**, then **Open decoder…** |
| [configs/](configs/) | Markdown configurations for CSV loading | **Decoder → CSV → Open configuration…** |
| [generators/](generators/) | Scripts that create sample data | Run from a terminal; outputs belong in `data/` |

For a larger presentation, use the [2 GB CSV and binary demos](waveform_demo_guide.md).
Both contain the same 12 waveform patterns, with matching configurations.

Small starting points:

- [sample.csv](data/sample.csv) with [sample_config.md](configs/sample_config.md).
  Its configured data path is relative to the project working directory.
  CSV opens its first portion automatically. Change CSV settings in **Decoder**,
  then **Apply** (F5); choose visible columns in **Plot → Columns**. Table navigation
  and **Plot → Next** request more rows. **Index entire file** is optional, and
  reopening restores the previously indexed portion. Click a table value or plotted
  sample to select its bytes in Hex; select bytes to reveal their row and plot marker.
  Choosing another decoder editor preserves the displayed CSV results.
- [sensors.bin](data/sensors.bin) with [sensors.yaml](decoders/yaml/sensors.yaml),
  also available through **File → Open sensor demo**.
- [waveform.py](decoders/python/waveform.py) and the equivalent
  [YAML decoder](decoders/yaml/waveform.yaml) for the generated tagged-container fixtures.
  The [40 GB fixture guide](waveform_stress_guide.md) describes the small previews,
  large data file, generator, and waveform checks.

The [generic YAML reference](generic_structures.md) stays here alongside the guides.
Large generated data files are disposable fixtures and are ignored by Git.
