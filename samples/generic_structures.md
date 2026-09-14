# Reusable structure definitions

These definitions use generic `structures` and `actions`; they are independent of
file extensions. Existing `sections` YAML remains a separate, compatible reader.
Open any file, load one of the example YAML definitions, edit it for that file,
and apply it. Hex remains available when no definition exists or a read fails.

## Reading the editor colors

The base app and browser share the same editor. YAML keys use blue, built-in type
values such as `u32`, `f32`, and `text` use teal, and user-defined type names use
the normal text color. The legend above the editor identifies these categories.
Quoted built-in type values and numeric type-choice mappings use the same teal.
Comments, literal strings, and a field merely named `u32` retain their own colors.
These are Binwave decoder types; YAML itself does not define them.

## Fields and simple actions

A structure contains `fields`, optional `actions` after its fields, and an optional
`endian` (`little` or `big`). A field has a `name` and `type`. The same field shape
is used by a `read` action. Built-in types are `u8`, `i8`, `u16`, `i16`, `u32`, `i32`,
`u64`, `i64`, `f32`, `f64`, `bytes`, UTF-8 `text`, zero-terminated UTF-8 `cstring`, and
unsigned LEB128 `varuint`. A named structure can be used wherever a type is used.
Integers remain exact; only plotting converts values to floating point.

Additional field inputs:

- `length`: byte length. Required for `bytes` and `text`. On a structure it bounds
  its contents and consumes any remaining bytes as padding. This lets an unknown
  section stay raw without losing the location of the next section.
- `repeat`: an item count or `until end` of the enclosing region. Length applies
  to each item, not to the entire collection.
- `at`: absolute file offset. `relative`: signed displacement from the current
  byte position. Use one or the other. Neither can escape a bounded region.
- `align`: advance to the next multiple of this many bytes before reading.
- `endian`: override inherited byte order.
- `expect`: require a numeric or text value, such as a signature.

A number can be a literal or `{ref: header.count}`. References look first in the
current element, then enclosing elements, then the file-level scope. `/header.count`
explicitly names a file-level value. References are to earlier decoded values;
forward references and arbitrary code execution are unsupported. Paths use named
fields and zero-based collection indices, e.g. `header.entries[0].size`. Individual
values inside a large deferred collection are not published until interpreted;
use the description collection itself with `define` when creating new structures.

Named calculations use checked integer arithmetic:

```yaml
- calculate:
    name: payload_bytes
    value: {multiply: [{ref: header.count}, 8]}
```

`add`, `subtract`, `multiply`, and `divide` each take two inputs. Division must be
exact. Negative lengths, counts and positions, overflow, missing references and
out-of-region reads are errors.

A field can choose its type using a value-to-type mapping:

```yaml
type:
  choose:
    ref: id
    cases: {'1': Header, '2': Data}
    otherwise: bytes
```

The `otherwise` type is optional; an unmatched value without one reports an error.
To run a different sequence of actions, use:

```yaml
- choose:
    ref: id
    cases:
      '1':
        - read: {name: header, type: Header}
    otherwise:
      - skip: {ref: length}
```

`skip` advances by a byte count. `align` advances to an absolute alignment boundary.
Calculations and choices are declarative data operations, not a general-purpose
programming language.

## Definitions created from data

`decoders/yaml/generic_sections.yaml` demonstrates sections that define the structure of a later
section. A description is an ordinary nested element, not a special file-format
concept. The `define` action selects an earlier collection with `from` and maps its
properties to fields in a new named structure:

```yaml
- define:
    name: Discovered
    from: metadata.entries
    fields:
      name: {ref: name}
      type: {ref: format, cases: {'1': u16, '2': i16}}
      repeat: {ref: count}
```

`name` is optional; omitted names become `field_0`, `field_1`, etc. Without a `cases`
mapping, `type: {ref: format}` uses the decoded text itself as a primitive or named
structure type. Optional `repeat`, `length`, `at`, and `relative` mappings are
resolved separately for each description. Newly created structures can be reused
by later reads, including reads within later nested sections. Structure names must
be unique, and definitions cannot silently overwrite an earlier definition.

A complete synthetic file for `decoders/yaml/generic_sections.yaml`, in hexadecimal:

```
01 0C 6C 65 66 74 01 02 72 67 68 74 02 01
02 06 FF FF 07 00 FE FF
```

The first section defines `left` as two `u16` values and `rght` as one `i16` value.
The next section decodes as `[65535, 7]` and `[-2]`. Changing the first description's
format byte from `01` to `02`, then interpreting the updated bytes, changes the
first value to `-1` without changing the definition file.

## Large files and current boundaries

Fixed-size collections over 128 items retain a lazy read plan and their total byte
range. Expanding a page reads at most 512 elements. Applying a definition does not
load an entire large fixed-size payload. Variable-size structures must be scanned
to discover their boundaries; parsing stops with a descriptive error beyond 20,000
nodes. Narrow the input region or express a fixed-size collection for larger data.
Description-derived structures are limited to 4,096 fields. YAML is limited to
256 KiB, text to 1 MiB per value, nesting to 64 levels, and parsing is cancellable.

`decoders/yaml/png_chunks.yaml` demonstrates signature checks and length-delimited chunks;
`decoders/yaml/riff_chunks.yaml` adds format selection, nested regions and alignment. They inspect
containers and metadata. Compressed streams, checksum verification/rebuilding,
bitfields, UTF-16, general CSV text tokenization in this reader, and variable-length
structured export are not yet implemented. CSV retains the application's existing
progressive CSV reader. Raw replacement and primitive field edits preserve length;
users must update dependent counts, offsets and checksums themselves when required.
