"""BinWave's standard-library Python reader and decoder-plan bridge.

Executed by BinWave, which supplies the open file (including byte edits).
User scripts import Field/Struct from binwave and use the global ``file``.
This is normal Python, not a security sandbox.
"""

import collections.abc
import dataclasses
import io
import json
import keyword
import linecache
import math
import operator
import re
import struct as _struct
import sys
import traceback
import types

READ_LIMIT = 1024 * 1024
CACHE_SIZE = 64 * 1024
OUTPUT_LIMIT = 64 * 1024
PROGRAM_LIMIT = 256 * 1024
MAX_SIZE = (1 << 63) - 1
MAX_SLICE = 512
MAX_ITERATION = 4096
_PRIMITIVES = {
    "u8": ("B", 1), "u16": ("H", 2), "u32": ("I", 4), "u64": ("Q", 8),
    "i8": ("b", 1), "i16": ("h", 2), "i32": ("i", 4), "i64": ("q", 8),
    "f32": ("f", 4), "f64": ("d", 8),
}
_RESERVED = set(_PRIMITIVES) | {"text", "bytes", "cstring", "varuint"}


def _name(value, label="name"):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,127}", value) or keyword.iskeyword(value):
        raise ValueError(f"{label} must be a Python identifier of at most 128 ASCII characters; got {value!r}")
    return value


def _size(value, label):
    if isinstance(value, bool):
        raise TypeError(f"{label} must be an integer, not bool")
    try:
        value = operator.index(value)
    except TypeError:
        raise TypeError(f"{label} must be an integer; got {value!r}") from None
    if not 0 <= value <= MAX_SIZE:
        raise ValueError(f"{label} must be between 0 and {MAX_SIZE}; got {value}")
    return value


def _endian(value):
    if value not in (None, "little", "big"):
        raise ValueError("endian must be 'little' or 'big'")
    return value


@dataclasses.dataclass(frozen=True)
class Field:
    """One field. ``length`` is bytes per item; ``count`` repeats that item.

    ``offset`` is the position within the containing Struct, useful for gaps.
    Use an explicit bytes field for named padding. Endianness is inherited unless
    specified here. Fixed-size text is UTF-8 and preserves embedded/trailing NULs.
    """
    name: str
    type: object
    length: object = None
    count: object = None
    offset: object = None
    endian: object = None
    expect: object = None

    def __post_init__(self):
        _name(self.name, "field name")
        if not isinstance(self.type, Struct) and self.type not in _RESERVED:
            raise ValueError(f"{self.name}: use a Struct or one of {', '.join(_PRIMITIVES)}, text, bytes")
        if self.type in ("cstring", "varuint"):
            raise ValueError(f"{self.name}: variable-width {self.type} needs Python read/unpack logic; emitted fields must have a fixed size")
        for key in ("length", "count", "offset"):
            value = getattr(self, key)
            if value is not None:
                object.__setattr__(self, key, _size(value, f"{self.name}.{key}"))
        _endian(self.endian)
        if self.type in ("bytes", "text") and self.length is None:
            raise ValueError(f"{self.name}: {self.type} requires length in bytes")
        if self.length is not None and self.length < self.natural_size:
            raise ValueError(f"{self.name}: length {self.length} is smaller than the {self.natural_size}-byte value")
        if self.expect is not None:
            if isinstance(self.type, Struct) or self.type == "bytes":
                raise ValueError(f"{self.name}: expect supports numeric or text fields; use a Python assertion to check raw bytes")
            if isinstance(self.expect, bool) or not isinstance(self.expect, (int, float, str)):
                raise TypeError(f"{self.name}: expect must be a number or string")
            if isinstance(self.expect, float) and not math.isfinite(self.expect):
                raise ValueError(f"{self.name}: expect must be finite")
            if isinstance(self.expect, int) and not -(1 << 63) <= self.expect <= (1 << 64) - 1:
                raise ValueError(f"{self.name}: expected integer is outside the supported 64-bit range")
        _size(self.size, f"{self.name} total byte size")

    @property
    def natural_size(self):
        if isinstance(self.type, Struct):
            return self.type.size
        return _PRIMITIVES.get(self.type, (None, 0))[1]

    @property
    def item_size(self):
        return self.natural_size if self.length is None else self.length

    @property
    def size(self):
        return self.item_size * (1 if self.count is None else self.count)


class Struct:
    """A reusable fixed-size structure. Tuples are shorthand for Field(name,type)."""
    def __init__(self, name, fields, endian=None):
        self.name = _name(name, "structure name")
        if name in _RESERVED:
            raise ValueError(f"structure name {name!r} is reserved for a built-in type")
        self.endian = _endian(endian)
        prepared = []
        positions = []
        seen = set()
        cursor = 0
        for item in fields:
            if len(prepared) >= 4096:
                raise ValueError(f"{name}: at most 4096 fields are supported")
            field = item if isinstance(item, Field) else Field(*item)
            if field.name in seen:
                raise ValueError(f"{name}: duplicate field {field.name!r}")
            seen.add(field.name)
            start = cursor if field.offset is None else field.offset
            if start < cursor:
                raise ValueError(f"{name}.{field.name}: offset {start} overlaps an earlier field; use a separate read_struct for overlapping interpretations")
            positions.append(start)
            prepared.append(field)
            cursor = _size(start + field.size, f"{name} byte size")
        self.fields = tuple(prepared)
        self._positions = tuple(positions)
        self.size = cursor

    def __repr__(self):
        return f"Struct({self.name!r}, {len(self.fields)} fields, size={self.size})"


class Record(dict):
    """Decoded metadata, accessible with record['field'] or record.field."""
    def __getattr__(self, name):
        try:
            return self[name]
        except KeyError:
            raise AttributeError(name) from None


class _Values(collections.abc.Sequence):
    def __init__(self, reader, start, field, count, endian, path):
        self._reader, self._start, self._field = reader, start, field
        self._count, self._endian, self._path = count, endian, path

    def __len__(self):
        return self._count

    def __iter__(self):
        # list(sequence) uses its length as an allocation hint. Reject accidental
        # whole-file materialization before an iterator (or allocation) is made.
        if self._count > MAX_ITERATION:
            raise ValueError(f"{self._path} has {self._count} items; read indexed items or slices of at most {MAX_SLICE} instead of iterating the whole collection")
        return (self[index] for index in range(self._count))

    def __getitem__(self, index):
        if isinstance(index, slice):
            indices = range(*index.indices(self._count))
            if len(indices) > MAX_SLICE:
                raise ValueError(f"slice contains {len(indices)} items; read at most {MAX_SLICE} at a time or use explicit indexes")
            return [self[item] for item in indices]
        index = operator.index(index)
        if index < 0:
            index += self._count
        if not 0 <= index < self._count:
            raise IndexError("record index is outside the collection")
        return self._reader._value(self._field, self._start + index * self._field.item_size,
                                   self._endian, f"{self._path}[{index}]", 0)

    def __repr__(self):
        return f"LazyRecords({self._path!r}, count={self._count}, offset={self._start})"


class Reader:
    """Bounded access to the app's current byte revision, plus emitted read plans.

    read_struct returns metadata immediately. records registers a collection for
    the app without reading all its samples. Both move the cursor past their range.
    read_at and sequence indexing leave the cursor unchanged.
    """
    def __init__(self, size, filename, read_bytes):
        self.size = _size(size, "file size")
        self._decoder_filename = filename
        self._read_bytes = read_bytes
        self._cursor = 0
        self._cache_start = 0
        self._cache = b""
        self._structures = {}
        self._actions = []
        self._names = set()

    def tell(self):
        return self._cursor

    def seek(self, offset, whence=0):
        offset = operator.index(offset)
        if whence not in (0, 1, 2):
            raise ValueError("whence must be 0 (start), 1 (cursor), or 2 (end)")
        target = (0, self._cursor, self.size)[whence] + offset
        self._cursor = self._range(target, 0)
        return self._cursor

    def _range(self, offset, length):
        offset = _size(offset, "byte offset")
        length = _size(length, "byte length")
        if offset > self.size or length > self.size - offset:
            raise ValueError(f"read at byte {offset} for {length} bytes exceeds file size {self.size}")
        return offset

    def read(self, size):
        value = self.read_at(self._cursor, size)
        self._cursor += len(value)
        return value

    def read_at(self, offset, size):
        size = _size(size, "read size")
        offset = self._range(offset, size)
        if size > READ_LIMIT:
            raise ValueError("read size exceeds 1 MiB; read in chunks or use records() for a lazy collection")
        if not size:
            return b""
        relative = offset - self._cache_start
        if relative >= 0 and relative + size <= len(self._cache):
            return self._cache[relative:relative + size]
        amount = min(self.size - offset, max(size, CACHE_SIZE))
        data = self._read_bytes(offset, amount)
        if not isinstance(data, bytes) or len(data) != amount:
            raise ValueError(f"file reader returned {len(data)} bytes at {offset}; expected {amount}")
        if amount <= CACHE_SIZE:
            self._cache_start, self._cache = offset, data
        return data[:size]

    def unpack(self, struct_fmt, offset=None):
        size = _struct.calcsize(struct_fmt)
        data = self.read(size) if offset is None else self.read_at(offset, size)
        return _struct.unpack(struct_fmt, data)

    def _register(self, definition, depth=0):
        if depth > 64:
            raise ValueError("nested structures exceed 64 levels")
        if not isinstance(definition, Struct):
            raise TypeError("read_struct and records require a Struct")
        fields = []
        cursor = 0
        names = {field.name for field in definition.fields}
        for index, (field, start) in enumerate(zip(definition.fields, definition._positions)):
            if start > cursor:
                # Explicit gaps become padding, preserving projection-friendly
                # contiguous fields for Rust's large-file plot queries.
                padding_name = f"_padding_{index}"
                while padding_name in names:
                    padding_name = "_" + padding_name
                names.add(padding_name)
                fields.append({"name": padding_name, "type": "bytes", "length": start - cursor})
            if isinstance(field.type, Struct):
                self._register(field.type, depth + 1)
                kind = field.type.name
            else:
                kind = field.type
            entry = {"name": field.name, "type": kind}
            if field.length is not None or isinstance(field.type, Struct):
                entry["length"] = field.item_size
            if field.count is not None:
                entry["repeat"] = field.count
            if field.endian is not None:
                entry["endian"] = field.endian
            if field.expect is not None:
                entry["expect"] = field.expect
            fields.append(entry)
            cursor = start + field.size
        if len(fields) > 4096:
            raise ValueError(f"{definition.name}: fields plus offset padding exceed 4096")
        serialized = {"fields": fields}
        if definition.endian is not None:
            serialized["endian"] = definition.endian
        previous = self._structures.get(definition.name)
        if previous is not None and previous != serialized:
            raise ValueError(f"structure {definition.name!r} was already emitted with different fields; use a distinct name")
        if previous is None and len(self._structures) >= 256:
            raise ValueError("a decoder may emit at most 256 distinct structures")
        self._structures[definition.name] = serialized

    def _emit(self, name, definition, offset, count=None):
        _name(name, "read name")
        if name in self._names:
            raise ValueError(f"read name {name!r} was already used; give each emitted read a unique name")
        if len(self._actions) >= 4096:
            raise ValueError("a decoder may emit at most 4096 reads; group samples with records()")
        self._register(definition)
        entry = {"name": name, "type": definition.name, "at": offset, "length": definition.size}
        if count is not None:
            entry["repeat"] = count
        self._actions.append({"read": entry})
        self._names.add(name)

    def read_struct(self, name, definition, offset=None):
        if not isinstance(definition, Struct):
            raise TypeError("read_struct requires a Struct")
        start = self._cursor if offset is None else offset
        start = self._range(start, definition.size)
        value = self._record(definition, start, "little", name, 0)
        self._emit(name, definition, start)
        self._cursor = start + definition.size
        return value

    def records(self, name, definition, count=None, offset=None):
        if not isinstance(definition, Struct):
            raise TypeError("records requires a Struct")
        start = self._range(self._cursor if offset is None else offset, 0)
        if definition.size == 0:
            raise ValueError("records requires a Struct with a positive byte size")
        if count is None:
            count, trailing = divmod(self.size - start, definition.size)
            if trailing:
                raise ValueError(f"{trailing} trailing bytes do not form a complete {definition.size}-byte {definition.name}; pass an explicit count for a bounded section")
        count = _size(count, "record count")
        self._range(start, _size(count * definition.size, "collection byte size"))
        self._emit(name, definition, start, count)
        self._cursor = start + count * definition.size
        return _Values(self, start, Field("item", definition), count, "little", name)

    def _record(self, definition, start, inherited, path, depth):
        if depth > 64:
            raise ValueError("nested metadata exceeds 64 levels")
        endian = definition.endian or inherited
        record = Record()
        for field, offset in zip(definition.fields, definition._positions):
            field_start = start + offset
            field_path = f"{path}.{field.name}"
            if field.count is None:
                record[field.name] = self._value(field, field_start, endian, field_path, depth + 1)
            else:
                record[field.name] = _Values(self, field_start, field, field.count, endian, field_path)
        return record

    def _value(self, field, offset, inherited, path, depth):
        if depth > 64:
            raise ValueError("nested metadata exceeds 64 levels")
        endian = field.endian or inherited
        if isinstance(field.type, Struct):
            # Field-specific byte order overrides the nested type's default.
            if field.endian is not None:
                record = Record()
                for child, child_offset in zip(field.type.fields, field.type._positions):
                    child_path = f"{path}.{child.name}"
                    if child.count is None:
                        record[child.name] = self._value(child, offset + child_offset, endian, child_path, depth + 1)
                    else:
                        record[child.name] = _Values(self, offset + child_offset, child, child.count, endian, child_path)
                return record
            return self._record(field.type, offset, inherited, path, depth + 1)
        if field.type in ("bytes", "text"):
            value = self.read_at(offset, field.item_size)
            if field.type == "text":
                value = value.decode("utf-8")
        else:
            fmt, width = _PRIMITIVES[field.type]
            value = _struct.unpack(("<" if endian == "little" else ">") + fmt,
                                   self.read_at(offset, width))[0]
        if field.expect is not None and value != field.expect:
            raise ValueError(f"{path} at byte {offset}: expected {field.expect!r}, found {value!r}")
        return value

    def program(self):
        if not self._actions:
            return None
        program = {"endian": "little", "structures": self._structures, "actions": self._actions}
        if len(json.dumps(program, ensure_ascii=False).encode("utf-8")) > PROGRAM_LIMIT:
            raise ValueError("emitted decoder exceeds 256 KiB; reuse Struct types and records() instead of emitting each sample")
        return program


class _CapturedOutput(io.TextIOBase):
    def __init__(self):
        self.data = bytearray()
        self.truncated = False

    @property
    def encoding(self):
        return "utf-8"

    def write(self, text):
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        remaining = OUTPUT_LIMIT - len(self.data)
        encoded = text[:remaining + 1].encode("utf-8", errors="replace")
        self.data.extend(encoded[:remaining])
        self.truncated |= len(encoded) > remaining or len(text) > remaining
        return len(text)

    def flush(self):
        pass

    def getvalue(self):
        if self.truncated:
            note = b"\n[Output truncated at 64 KiB]\n"
            return self.data[:OUTPUT_LIMIT - len(note)].decode("utf-8", errors="ignore") + note.decode()
        return self.data.decode("utf-8", errors="replace")


def _main():
    protocol_in, protocol_out = sys.stdin, sys.stdout
    captured = _CapturedOutput()

    def send(value):
        protocol_out.write(json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":")) + "\n")
        protocol_out.flush()

    def read_bytes(offset, length):
        send({"read": {"offset": offset, "length": length}})
        line = protocol_in.readline(4 * READ_LIMIT + 65536)
        if not line or not line.endswith("\n"):
            raise RuntimeError("BinWave file reader closed or returned an oversized response")
        reply = json.loads(line)
        if "error" in reply:
            raise OSError(reply["error"])
        data = bytes(reply["data"])
        if len(data) != length:
            raise OSError(f"BinWave returned {len(data)} bytes; requested {length}")
        return data

    try:
        line = protocol_in.readline(2 * READ_LIMIT + 1)
        if not line or not line.endswith("\n"):
            raise ValueError("missing or oversized decoder request")
        request = json.loads(line)
        script = request["script"]
        if not isinstance(script, str) or len(script.encode("utf-8")) > PROGRAM_LIMIT:
            raise ValueError("Python decoder must be text of at most 256 KiB")
        reader = Reader(request["size"], request["filename"], read_bytes)
        sdk = types.ModuleType("binwave", "Read file bytes and describe reusable structures for BinWave.")
        sdk.Field, sdk.Struct, sdk.Record, sdk.file = Field, Struct, Record, reader
        sdk.__all__ = ["Field", "Struct", "Record", "file"]
        sys.modules["binwave"] = sdk
        sys.stdout = sys.stderr = captured
        source_name = request["filename"]
        if not isinstance(source_name, str) or not source_name:
            raise ValueError("decoder filename must be a nonempty string")
        linecache.cache[source_name] = (len(script), None, script.splitlines(keepends=True), source_name)
        exec(compile(script, source_name, "exec"), {"__name__": "__main__", "__file__": source_name, "file": reader})
        result = {"done": {"program": reader.program(), "stdout": captured.getvalue()}}
    except BaseException:
        error = _CapturedOutput()
        error.write(traceback.format_exc(limit=20))
        result = {"error": error.getvalue(), "stdout": captured.getvalue()}
    finally:
        sys.stdout, sys.stderr = protocol_out, sys.__stderr__
    send(result)


if __name__ == "__main__":
    _main()
