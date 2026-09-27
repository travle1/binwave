# BinWave decoder for RTLM telemetry, sectioned layout.
# Verified against BinWave's Python runner and read-plan engine on 5 min and
# 6 h (982 MB) files. Like the PWAV demo, nothing about the streams is
# hardcoded: every record type is built from the descriptors stored in the file.
from binwave import Field, Struct

FORMATS = {1: "u64", 2: "i64", 3: "f64", 4: "f32", 5: "u32", 6: "i32",
           7: "u16", 8: "u8", 9: "i16"}

Header = Struct("Header", [
    Field("magic", "string", length=4, expect="RTLM"),
    Field("version", "u16"),
    Field("layout", "u8"),            # 0 interleaved, 1 sectioned
    Field("stream_count", "u8"),
    Field("data_offset", "u32"),
    Field("start_unix_us", "u64"),
    Field("duration_us", "u64"),
    Field("file_bytes", "u64"),
    Field("total_records", "u64"),
    Field("seed", "u32"),
    Field("reserved", "bytes", length=16),
], endian="little")

StreamDesc = Struct("StreamDesc", [
    Field("stream_id", "u8"),
    Field("field_count", "u8"),
    Field("rate_hz", "u16"),
    Field("payload_bytes", "u16"),
    Field("reserved", "u16"),
    Field("name", "string", length=16),
], endian="little")

FieldDesc = Struct("FieldDesc", [
    Field("name", "string", length=16),
    Field("format", "u8"),
    Field("byte_width", "u8"),
    Field("reserved", "u16"),
], endian="little")

SectionHeader = Struct("SectionHeader", [
    Field("stream_id", "u32"),
    Field("record_bytes", "u32"),
    Field("record_count", "u64"),
], endian="little")


def record_type(name, field_descs):
    fields = [Field("sync", "u16", expect=0xA55A), Field("stream_id", "u8"),
              Field("seq", "u8"), Field("t_us", "u64")]
    for d in field_descs:
        fields.append(Field(d.name.rstrip("\0"), FORMATS[d.format], length=d.byte_width))
    fields.append(Field("crc", "u16"))
    return Struct(name, fields, endian="little")


header = file.read_struct("header", Header)
assert header.file_bytes == file.size, "file truncated or header not finalized"
assert header.layout == 1, "interleaved layout: records are not contiguous per stream"

types, names = {}, {}
for i in range(header.stream_count):
    sd = file.read_struct(f"stream_desc_{i}", StreamDesc)
    fds = file.records(f"fields_{i}", FieldDesc, count=sd.field_count)
    name = sd.name.rstrip("\0")
    types[sd.stream_id] = record_type(name, fds)
    names[sd.stream_id] = name

file.seek(header.data_offset)
for i in range(header.stream_count):
    sh = file.read_struct(f"section_{i}", SectionHeader)
    assert sh.record_bytes == types[sh.stream_id].size, "record size mismatch"
    # registers the collection lazily and advances past it; nothing is read yet
    file.records(names[sh.stream_id], types[sh.stream_id], count=sh.record_count)
