# Reusable structure definitions

These definitions use generic `structures` and `actions`; they are independent of
file extensions. Existing `sections` YAML remains a separate, compatible reader.
Open any file, load one of the example YAML definitions, edit it for that file,
and apply it. Hex remains available when no definition exists or a read fails.

## Explicit declarations and references

The runnable examples use the same language in the base app and browser.
Declare structures as a list with `name:` and reference them with
`type: {structure: Position}`. A bare `type: u16` means a built-in type.
Mapping-style declarations (`Position:` beneath `structures`) and bare custom
type references (`type: Position`) are rejected. This is a breaking redesign,
not a second syntax or a compatibility shim. Update saved decoder files before
applying them; the editor does not silently rewrite your files or saved drafts.

A structure reference is explicit everywhere, including `choose.cases`,
`choose.otherwise`, and type mappings used by `define`. For example:

```yaml
type:
  choose:
    ref: id
    cases:
      '1': {structure: Position}
      '2': u16
    otherwise: bytes
```

In a `define` action with no cases, a decoded type name is data from the file,
not a YAML type reference. It still resolves through the same type catalog.
Inline nested `fields` without a `type` is not supported: declare a named
structure and reference it explicitly.

## Three different kinds of nesting

| Layer | What it means | Does it read bytes? |
| --- | --- | --- |
| YAML nesting | Which settings belong to a declaration, field, or action | No, by itself |
| Type declarations under `structures` | Reusable instructions for interpreting one value | No, until a read uses the type |
| Decoded nesting | A field's child values, including nested structures and repeated items | Yes, when the corresponding read runs |

`structures` is a type catalog, not a description of the file's top-level order.
Two entries alongside one another do not mean they occur consecutively in the
file. Moving a declaration above another declaration does not move its data.
The order of `actions` determines top-level reads. Within a structure, the order
of `fields` determines field reads, followed by that structure's `actions`.

The settings keys (`fields`, `type`, `repeat`, `ref`, etc.) are built into
Binwave's decoder language. YAML itself does not give those words decoding
semantics. Names such as `Position`, `Record`, `position`, and `records` are
chosen by the decoder author. They are not reserved file-format concepts.

## Worked example: declarations → bytes → decoded paths

This is the complete runnable [nested records decoder](decoders/yaml/nested_records.yaml):

```yaml
endian: little
structures:
  - name: Position
    fields:
      - {name: x, type: u16}
      - {name: y, type: u16}
  - name: Record
    fields:
      - {name: id, type: u8}
      - {name: position, type: {structure: Position}}
actions:
  - read:
      name: records
      type: {structure: Record}
      repeat: 2
```

Use these ten bytes as the input file (spaces are only for readability):

```text
07 0A 00 14 00  08 1E 00 28 00
```

| Byte offsets, inclusive | Bytes | Type | Decoded path | Value |
| --- | --- | --- | --- | ---: |
| 0 | `07` | `u8` | `records[0].id` | 7 |
| 1–2 | `0A 00` | `u16` | `records[0].position.x` | 10 |
| 3–4 | `14 00` | `u16` | `records[0].position.y` | 20 |
| 5 | `08` | `u8` | `records[1].id` | 8 |
| 6–7 | `1E 00` | `u16` | `records[1].position.x` | 30 |
| 8–9 | `28 00` | `u16` | `records[1].position.y` | 40 |

The decoded hierarchy is:

```text
records
├── [0]
│   ├── id = 7
│   └── position
│       ├── x = 10
│       └── y = 20
└── [1]
    ├── id = 8
    └── position
        ├── x = 30
        └── y = 40
```

`Position` is a reusable type name; `position` is the name of one field using
that type. The field creates the nesting. `records` names the top-level read,
and `repeat: 2` creates its indexed children. Type names are not automatically
inserted into decoded paths: the path contains `position`, not `Position`.

**The raw bytes do not contain this hierarchy or these names. The decoder assigns
them.** No opening/closing bytes mark `Position`: its two `u16` fields consume
four bytes, then reading continues in the containing `Record`. Each record
therefore occupies five bytes. There is no automatic native-struct padding or
alignment. Little-endian byte order makes `0A 00` equal to 10, not 2560.

## The byte cursor, bounds, and repetition

The byte cursor starts at zero. An ordinary field begins at the current cursor,
consumes its bytes, and leaves the cursor for the next field. Entering a nested
structure does not reset the cursor to zero. Returning to its parent does not
restore the cursor to where that nested read began.

| Setting | Effect on reading |
| --- | --- |
| `repeat: 2` | Read two consecutive items, not two bytes |
| `repeat: {ref: count}` | Read the number of items given by an earlier decoded value |
| `repeat: until end` | Read complete items until the enclosing byte region ends; at top level this is the file end |
| `length: 12` | Bound this item to 12 bytes; for a structure, unused bytes are consumed as padding |
| `at: 100` | Start at absolute file byte 100, even inside a nested structure |
| `relative: 4` | Start four bytes after the current cursor |
| `align: 4` | Advance to the next absolute offset divisible by four |

`at` and `relative` are alternatives. An explicit position changes where reading
continues; it is not a temporary peek that restores the previous cursor.
Reads cannot escape an explicitly bounded parent region. Field lengths and
offsets are byte quantities; repeat counts are item quantities.

**A field's `length` applies to each repeated item, not the array as a whole.**
For example, `type: {structure: Record}, length: 8, repeat: 2` reserves eight bytes per record,
for sixteen bytes total. To bound a whole repeated payload, give `length` to a
containing structure and put the repeated field inside it. A trailing partial
item is an error, not an instruction to silently discard bytes. Likewise,
`until end` cannot identify a following footer unless the payload has its own
boundary that excludes that footer.

Nesting does not imply a stored count, byte length, pointer, tag, or terminator.
Declare and use those only if the file format actually contains them. The
interpreter does not infer field layouts from arbitrary bytes.

## References name decoded values, not types

`ref` retrieves an earlier value; it does not read additional bytes. For example,
`{ref: /header.count}` retrieves a decoded field called `count` beneath a
file-level read called `header`. It does not refer to the `Header` declaration.
An unqualified `{ref: count}` searches the current element and its enclosing
scopes. A leading `/` anchors the reference to the file level. See the detailed
reference rules below, including restrictions for lazy collections.

A name like `field_count` has no special meaning. If declared as `type: u16`, its
value is read from two bytes in the file. The author need not know that value
before opening the file, but must know where and how it is encoded. If no count
is stored, the format needs another known boundary or supported termination
rule. Unknown count plus unknown boundary is format investigation, not a
reliable repetition instruction.

## Ordinary nesting versus types generated from file data

Ordinary nesting uses a type already declared in the YAML, as `Record` uses
`Position` above. Dynamic type generation adds a separate sequence:

1. Read ordinary values that describe a record's fields, such as labels and
   format codes. A user-defined type might be named `RecordType` for this part
   of the file; that name is not a built-in instruction.
2. Run `define` to turn those decoded descriptions into a new named type.
   Creating the type does not itself read the later record payload.
3. Run a later `read` using that new type to consume the payload bytes.

Names in a generated type can come from the file, unlike the literal names in
our simple example. Format-code meanings still come from the decoder's mapping:
`'1': i16` is the author's rule, not a universal meaning for byte value 1.
`choose` selects a type or action branch using an earlier value; it does not
create a type. `define` creates the type. See the complete dynamic example below.

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
    cases: {'1': {structure: Header}, '2': {structure: Data}}
    otherwise: bytes
```

The `otherwise` type is optional; an unmatched value without one reports an error.
To run a different sequence of actions, use:

```yaml
- choose:
    ref: id
    cases:
      '1':
        - read: {name: header, type: {structure: Header}}
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
