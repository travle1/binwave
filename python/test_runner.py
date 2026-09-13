"""Run with python3 -m unittest discover -s python -p 'test_*.py'."""
import contextlib
import io
import json
import math
from pathlib import Path
import struct
import subprocess
import sys
import types
import unittest

import runner

ROOT = Path(__file__).resolve().parents[1]


# Independent encoder for the generic tagged-container sample. Numeric codes are
# part of this fixture's recipe; the application itself has no such built-in map.
WAVEFORM_FIELDS = [
    ("sine", 3, "d"), ("triangle", 3, "d"), ("ramp", 3, "d"), ("chirp", 3, "d"),
    ("square", 4, "f"), ("pulse", 4, "f"), ("staircase", 4, "f"),
    ("damped_sine", 4, "f"), ("zigzag", 6, "i"), ("alternating", 6, "i"),
    ("counter_ramp", 5, "I"), ("bit_pattern", 5, "I"),
]


def waveform_values(index):
    phase = (index % 128) / 128
    bits = index % 16
    values = [
        4 * math.sin(math.tau * (index % 64) / 64),
        10 + 4 * (1 - 4 * abs((index % 96) / 96 - 0.5)),
        20 + 8 * ((index % 128) / 127 - 0.5),
        30 + 4 * math.sin(math.tau * (phase + 3 * phase * phase)),
        40 + (3 if index % 64 < 32 else -3),
        50 + (6 if index % 96 < 8 else 0),
        60 + (index % 128) // 16,
        70 + 4 * math.exp(-3 * phase) * math.sin(math.tau * 4 * phase),
        80 + abs(index % 32 - 16) // 2,
        100 + (3 if (index // 8) % 2 == 0 else -3),
        110 + bits,
        130 + (bits ^ (bits >> 1)),
    ]
    return {name: struct.unpack("<" + fmt, struct.pack("<" + fmt, value))[0]
            for (name, _, fmt), value in zip(WAVEFORM_FIELDS, values)}


def waveform_fixture(count, data_offset, reversed_fields=False):
    fields = list(reversed(WAVEFORM_FIELDS)) if reversed_fields else WAVEFORM_FIELDS
    stride = sum(struct.calcsize("<" + fmt) for _, _, fmt in fields)
    data = bytearray(data_offset + count * stride + 64)
    struct.pack_into("<4sIQQII", data, 0, b"PWAV", 1, len(data), count, 4, len(fields))
    struct.pack_into("<IIQ", data, 32, 1, 0, len(fields) * 24)
    for position, (name, code, fmt) in enumerate(fields):
        struct.pack_into("<16sII", data, 48 + position * 24,
                         name.encode(), code, struct.calcsize("<" + fmt))
    unknown_start = 48 + len(fields) * 24
    unknown_end = data_offset - 16
    struct.pack_into("<IIQ", data, unknown_start, 0x7fffffff, 0,
                     unknown_end - unknown_start - 16)
    data[unknown_start + 16:unknown_end] = b"\xa5" * (unknown_end - unknown_start - 16)
    struct.pack_into("<IIQ", data, data_offset - 16, 2, 0, count * stride)
    for index in range(count):
        values = waveform_values(index)
        struct.pack_into("<" + "".join(fmt for _, _, fmt in fields),
                         data, data_offset + index * stride,
                         *(values[name] for name, _, _ in fields))
    footer = data_offset + count * stride
    struct.pack_into("<IIQ8sQQQQQ", data, footer, 3, 0, 48, b"PWAV-END",
                     count, count * stride, data_offset, count - 1, 0)
    return bytes(data)


class ReaderTests(unittest.TestCase):
    def reader(self, data):
        calls = []
        def read_at(offset, size):
            calls.append((offset, size))
            return data[offset:offset + size]
        return runner.Reader(len(data), "test.bin", read_at), calls

    def test_header_values_drive_lazy_collection_and_exact_source_offsets(self):
        reader, calls = self.reader(bytes([2]) + struct.pack("<HH", 12, 99))
        header_type = runner.Struct("Header", [("count", "u8")])
        sample_type = runner.Struct("Sample", [("value", "u16")])
        header = reader.read_struct("header", header_type)
        samples = reader.records("samples", sample_type, count=header.count)
        self.assertEqual(reader.tell(), 5)
        self.assertEqual(samples[-1].value, 99)
        self.assertEqual([row.value for row in samples[:2]], [12, 99])
        self.assertEqual(reader.tell(), 5, "indexing must not move the file cursor")
        self.assertEqual(calls, [(0, 5)])
        read = reader.program()["actions"][1]["read"]
        self.assertEqual(read, {"name": "samples", "type": "Sample", "at": 1, "length": 2, "repeat": 2})

    def test_large_collection_registration_performs_no_reads(self):
        calls = []
        def source(offset, size):
            calls.append((offset, size))
            return bytes(size)
        reader = runner.Reader(40_000_000_000, "large.bin", source)
        rows = reader.records("samples", runner.Struct("Sample", [("value", "u64")]))
        self.assertEqual(len(rows), 5_000_000_000)
        self.assertEqual(calls, [])
        self.assertEqual(rows[-1].value, 0)
        self.assertEqual(calls, [(39_999_999_992, 8)])
        with self.assertRaisesRegex(ValueError, "at most 512"):
            rows[:]
        with self.assertRaisesRegex(ValueError, "instead of iterating"):
            list(rows)
        self.assertEqual(len(calls), 1)

    def test_nested_structures_padding_and_byte_order_match_plan(self):
        inner = runner.Struct("Inner", [("value", "u16")], endian="big")
        outer = runner.Struct("Outer", [
            runner.Field("first", "u8"),
            runner.Field("nested", inner, offset=4, endian="little"),
            runner.Field("tail", "text", length=2),
        ], endian="big")
        reader, _ = self.reader(bytes([1, 0, 0, 0, 0x34, 0x12, 65, 0]))
        value = reader.read_struct("header", outer)
        self.assertEqual(value.nested.value, 0x1234)
        self.assertEqual(value.tail, "A\0")
        fields = reader.program()["structures"]["Outer"]["fields"]
        self.assertEqual(fields[1], {"name": "_padding_1", "type": "bytes", "length": 3})
        self.assertEqual(fields[2]["length"], 2)
        self.assertEqual(fields[2]["endian"], "little")
        self.assertEqual(outer.size, 8)

    def test_raw_operations_bound_reads_and_leave_no_implicit_output(self):
        reader, _ = self.reader(struct.pack("<HH", 12, 99))
        self.assertEqual(reader.unpack("<H"), (12,))
        self.assertEqual(reader.unpack("<H", offset=2), (99,))
        self.assertEqual(reader.tell(), 2)
        self.assertEqual(reader.seek(-1, 2), 3)
        self.assertEqual(reader.read(1), b"\x00")
        self.assertIsNone(reader.program())
        with self.assertRaisesRegex(ValueError, "exceeds file size"):
            reader.read(1)
        reader = runner.Reader(2 * runner.READ_LIMIT, "large", lambda a, b: bytes(b))
        with self.assertRaisesRegex(ValueError, "exceeds 1 MiB"):
            reader.read(runner.READ_LIMIT + 1)

    def test_invalid_names_counts_layouts_and_expected_values_are_actionable(self):
        with self.assertRaisesRegex(ValueError, "identifier"):
            runner.Struct("bad name", [])
        with self.assertRaisesRegex(ValueError, "requires length"):
            runner.Field("text", "text")
        with self.assertRaisesRegex(ValueError, "overlaps"):
            runner.Struct("Bad", [("first", "u64"), runner.Field("next", "u8", offset=1)])
        with self.assertRaisesRegex(ValueError, "smaller"):
            runner.Field("value", "u64", length=4)
        reader, _ = self.reader(bytes([1, 2, 3]))
        sample = runner.Struct("Sample", [("value", "u16")])
        with self.assertRaisesRegex(ValueError, "trailing bytes"):
            reader.records("samples", sample)
        with self.assertRaisesRegex(ValueError, "record count"):
            reader.records("samples", sample, count=-1)
        reader.records("samples", sample, count=1)
        with self.assertRaisesRegex(ValueError, "already used"):
            reader.records("samples", sample, count=0)
        with self.assertRaisesRegex(ValueError, "expected 9, found 1"):
            reader.read_struct("checked", runner.Struct("Checked", [runner.Field("value", "u8", expect=9)]), offset=0)

    def test_fixture_script_discovers_reordered_channels_and_shifted_payload(self):
        script_path = ROOT / "samples/decoders/python/waveform.py"
        script = script_path.read_text()
        decoded = []
        for reversed_fields, data_offset in [(False, 4096), (True, 8192)]:
            # Build independent bytes in memory: this test needs no local data artifact.
            data = waveform_fixture(16_319, data_offset, reversed_fields)
            reader, calls = self.reader(data)
            sdk = types.ModuleType("plottermax")
            sdk.Field, sdk.Struct, sdk.file = runner.Field, runner.Struct, reader
            previous = sys.modules.get("plottermax")
            sys.modules["plottermax"] = sdk
            try:
                scope = {"file": reader}
                with contextlib.redirect_stdout(io.StringIO()):
                    exec(compile(script, str(script_path), "exec"), scope)
            finally:
                if previous is None:
                    sys.modules.pop("plottermax", None)
                else:
                    sys.modules["plottermax"] = previous
            result = scope["samples"]
            decoded.append((scope["sample_offset"], dict(result[17]), reader.program()))
            self.assertLess(sum(size for _, size in calls), 200_000)
            self.assertEqual(len(result), 16_319)
            self.assertEqual(scope["sample_offset"], data_offset)
            self.assertEqual(dict(result[17]), waveform_values(17))
            self.assertEqual(dict(result[-1]), waveform_values(16_318))
        self.assertEqual(decoded[0][1], decoded[1][1])
        first_fields = decoded[0][2]["structures"]["Sample"]["fields"]
        reversed_fields = decoded[1][2]["structures"]["Sample"]["fields"]
        self.assertEqual(first_fields, list(reversed(reversed_fields)))


class ProtocolTests(unittest.TestCase):
    def run_script(self, script, data=b"\x07\x00"):
        process = subprocess.Popen([sys.executable, str(ROOT / "python/runner.py")],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            process.stdin.write(json.dumps({"script": script, "filename": "decoder.py", "size": len(data)}) + "\n")
            process.stdin.flush()
            while True:
                line = process.stdout.readline()
                self.assertTrue(line, "runner exited before reporting a result")
                message = json.loads(line)
                if "read" not in message:
                    process.wait(timeout=5)
                    return message
                request = message["read"]
                offset, length = request["offset"], request["length"]
                self.assertLessEqual(length, runner.READ_LIMIT)
                process.stdin.write(json.dumps({"data": list(data[offset:offset + length])}) + "\n")
                process.stdin.flush()
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)

    def test_plain_python_and_raw_reads_succeed_with_explicit_null_program(self):
        for script, expected_stdout in [
            ("test = 5", ""),
            ("import math\ndef squared(x): return x*x\nprint(squared(3), math.sqrt(16))", "9 4.0\n"),
            ("from plottermax import file\nprint(file.read_at(0, 2).hex())", "0700\n"),
        ]:
            with self.subTest(script=script):
                result = self.run_script(script)
                self.assertEqual(result, {"done": {"program": None, "stdout": expected_stdout}})

    def test_invalid_raw_read_is_still_a_script_error(self):
        result = self.run_script("print('reading')\nfile.read_at(1, 2)")
        self.assertIn("ValueError", result["error"])
        self.assertIn("exceeds file size", result["error"])
        self.assertEqual(result["stdout"], "reading\n")
        self.assertNotIn("done", result)

    def test_regular_python_imports_functions_and_print_are_separate_from_protocol(self):
        result = self.run_script("from plottermax import Struct, file\nimport math\ndef squared(x): return x*x\nprint(squared(3), math.sqrt(16))\nfile.read_struct('sample', Struct('Sample', [('value','u16')]))")
        self.assertEqual(result["done"]["stdout"], "9 4.0\n")
        self.assertEqual(result["done"]["program"]["actions"][0]["read"]["length"], 2)

    def test_errors_have_script_traceback_and_prior_print_output(self):
        result = self.run_script("print('before error')\nraise ValueError('bad header')")
        self.assertIn('File "decoder.py", line 2', result["error"])
        self.assertIn("ValueError: bad header", result["error"])
        self.assertEqual(result["stdout"], "before error\n")

    def test_print_output_is_bounded(self):
        result = self.run_script("from plottermax import Struct\nprint('x' * 100000)\nfile.records('samples',Struct('Sample',[('value','u8')]))")
        output = result["done"]["stdout"]
        self.assertLessEqual(len(output.encode()), runner.OUTPUT_LIMIT)
        self.assertIn("Output truncated", output)


if __name__ == "__main__":
    unittest.main()
