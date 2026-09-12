# Sample files

| Folder | Contents | Open with |
| --- | --- | --- |
| [data/](data/) | CSV and binary data, including generated large fixtures | **File → Open file…** |
| [decoders/python/](decoders/python/) | Python decoder scripts | Select **Python**, then **Open decoder…** |
| [decoders/yaml/](decoders/yaml/) | YAML decoders and structure examples | Select **YAML**, then **Open decoder…** |
| [configs/](configs/) | Markdown configurations for CSV loading | **File → Load CSV configuration…** |
| [generators/](generators/) | Scripts that create sample data | Run from a terminal; outputs belong in `data/` |

Small starting points:

- [sample.csv](data/sample.csv) with [sample_config.md](configs/sample_config.md).
  Its configured data path is relative to the project working directory.
- [sensors.bin](data/sensors.bin) with [sensors.yaml](decoders/yaml/sensors.yaml),
  also available through **File → Open sensor demo**.
- [waveform.py](decoders/python/waveform.py) and the equivalent
  [YAML decoder](decoders/yaml/waveform.yaml) for the generated tagged-container fixtures.
  The [40 GB fixture guide](waveform_stress_guide.md) describes the small previews,
  large data file, generator, and waveform checks.

The [generic YAML reference](generic_structures.md) stays here alongside the guides.
Large generated data files are disposable fixtures and are ignored by Git.
