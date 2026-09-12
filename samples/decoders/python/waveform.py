"""Decode the generic PWAV waveform fixture, including reordered descriptors.

PWAV is a synthetic tagged container designed to exercise reusable decoders.
Python discovers the sections and sample structure; file.records() registers the
large byte-backed collection without loading its values into Python or RAM.
"""
from plottermax import Field, Struct

Header = Struct("Header", [
    Field("signature", "text", length=4, expect="PWAV"),
    Field("version", "u32", expect=1),
    ("file_bytes", "u64"),
    ("sample_count", "u64"),
    ("section_count", "u32"),
    ("descriptor_count", "u32"),
], endian="little")

SectionHeader = Struct("SectionHeader", [
    ("id", "u32"), ("flags", "u32"), ("payload_bytes", "u64"),
], endian="little")

Descriptor = Struct("Descriptor", [
    Field("name", "text", length=16),
    ("format", "u32"), ("byte_width", "u32"),
], endian="little")

Footer = Struct("Footer", [
    Field("marker", "text", length=8, expect="PWAV-END"),
    ("sample_count", "u64"), ("payload_bytes", "u64"),
    ("sample_offset", "u64"), ("final_index", "u64"),
    ("generator_seed", "u64"),
], endian="little")

FORMATS = {1: "u64", 2: "i64", 3: "f64", 4: "f32", 5: "u32", 6: "i32"}


def sample_type(descriptors):
    fields = []
    for descriptor in descriptors:
        kind = FORMATS.get(descriptor.format)
        if kind is None:
            raise ValueError(f"Unknown numeric format code {descriptor.format}")
        fields.append(Field(descriptor.name.rstrip("\0"), kind,
                            length=descriptor.byte_width))
    return Struct("Sample", fields, endian="little")


header = file.read_struct("header", Header)
assert header.file_bytes == file.size, "Header size does not match this file"
Sample = None
sample_count = 0
sample_offset = None
seen_sections = set()

for index in range(header.section_count):
    section = file.read_struct(f"section_{index}", SectionHeader)
    payload_start = file.tell()
    payload_end = payload_start + section.payload_bytes
    if payload_end > file.size:
        raise ValueError(f"Section {index} extends beyond the file")

    if section.id == 1:
        assert 1 not in seen_sections, "Duplicate descriptor section"
        assert section.payload_bytes == header.descriptor_count * Descriptor.size, "Invalid descriptor section length"
        descriptors = file.records("descriptors", Descriptor,
                                   count=header.descriptor_count)
        Sample = sample_type(descriptors)
        print(f"Discovered {len(Sample.fields)} fields, {Sample.size} bytes per sample")

    elif section.id == 2:
        assert 2 not in seen_sections, "Duplicate sample section"
        assert Sample is not None, "Sample descriptors must precede the sample section"
        assert section.payload_bytes == header.sample_count * Sample.size, "Sample count and section length disagree"
        samples = file.records("samples", Sample, count=header.sample_count)
        sample_count = len(samples)
        sample_offset = payload_start
        print(f"Registered {sample_count:,} samples at byte {sample_offset:,}; values load on demand")

    elif section.id == 3:
        assert 3 not in seen_sections, "Duplicate footer section"
        assert section.payload_bytes == Footer.size, "Invalid footer section length"
        footer = file.read_struct("footer", Footer)
        assert footer.sample_count == sample_count, "Footer sample count does not match"
        assert footer.sample_offset == sample_offset, "Footer sample offset does not match"
        assert footer.payload_bytes == sample_count * Sample.size, "Footer payload length does not match"
        assert footer.final_index == sample_count - 1, "Footer final sample index does not match"

    elif section.payload_bytes:
        # Unknown length-delimited sections still appear in the decoded structure.
        # Registering a raw record avoids copying an arbitrarily large payload.
        raw = Struct(f"RawSection{index}", [Field("payload", "bytes", length=section.payload_bytes)])
        file.records(f"unknown_section_{index}", raw, count=1)

    seen_sections.add(section.id)
    file.seek(payload_end)

assert {1, 2, 3} <= seen_sections, "Missing descriptors, samples, or footer"
assert file.tell() == file.size, "Unexpected bytes after the final section"
print("Ready: choose Show samples or Show plot.")
