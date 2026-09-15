Your first Binwave plot

Web app: tap Open simple demo in the empty workspace, or choose
Examples > Open simple demo · data + decoder.
Desktop: choose File > Open simple demo.

The 2 KB data file and matching decoder are embedded in the app. The demo
opens and plots automatically; no file picker or separate Apply is needed.

To practice opening your own files instead:
1. Open simple_wave.bin as the DATA file (Open file / Open data).
2. Choose Examples > Start here · Simple wave, or open simple_wave.yaml.
3. Apply.

Expect eight smooth sine-wave cycles, with values from -1 to +1.
The horizontal axis is the sample index, 0 through 511, not seconds.
In Decoded samples, sample 16 is +1 and sample 48 is -1.
Sample 16 occupies bytes 64 through 67 in Hex (4 bytes per sample).

The file is 2,048 bytes: 512 little-endian float32 values, no header or footer.
It is synthetic data, generated as sin(2*pi*index/64), rounded to float32.

How the decoder describes it:
- endian: little specifies the byte order.
- Sample is a name we chose for a structure containing one field.
- value is a name we chose for that field; f32 is a built-in 4-byte float type.
- samples is the name of the resulting collection.
- type: {structure: Sample} refers to the structure declared above.
- repeat: until end reads complete samples to the end of the file.
  It does not require a count stored in the file.

The default YAML is this same decoder. Existing saved drafts are preserved;
choose the example explicitly if your editor still shows an older decoder.
Other data files need a decoder matching their actual byte format.

Next steps
Try changing a sample value through Hex and observe the plot.
Explore Nested records for multiple fields and nested data.
The descriptor-driven waveform container is an advanced example.

Common formats
Examples includes PNG chunk structure and WAV/RIFF structure inspection.
PNG: chunk boundaries, image dimensions and metadata; compressed image bytes
remain raw. CRCs are displayed, not verified or updated.
WAV: RIFF chunks and format metadata; audio payloads remain raw bytes.
Neither decoder reconstructs an image or plays/plots decoded audio.

Advanced demo: File > Open advanced demo (native), or
Open data > Open advanced demo (web).
The bundled advanced_wave.bin is 8,080 bytes. It contains three collections:
384 waveform records, 192 measurements, and 96 events, with different field types.
The app applies samples/advanced/decoder.yaml automatically. Each collection
starts at sample index zero. Plot data toggles collections; the legend toggles fields.
Regenerate with python3 samples/generators/generate_advanced_demo.py.
The multi-gigabyte waveform stress fixtures remain local and are not bundled.
