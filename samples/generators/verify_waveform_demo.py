#!/usr/bin/env python3
"""Read-only, bounded checks of matching PWAV binary and CSV waveform demos.

Checks metadata, the first waveform period, the last rows, and distributed probes.
This is intentionally not a full-file checksum or a scan of every CSV row.
"""

import argparse
import csv
import math
from pathlib import Path
import struct
import sys
import tempfile
import unittest

PERIOD = 384
MAX_LINE_BYTES = 4096
CHANNELS = (
    "sine", "triangle", "ramp", "chirp", "square", "pulse", "staircase",
    "damped_sine", "zigzag", "alternating", "counter_ramp", "bit_pattern",
)
FORMATS = {1: "Q", 2: "q", 3: "d", 4: "f", 5: "I", 6: "i"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_exact(source, count):
    data = source.read(count)
    require(len(data) == count, "Unexpected end of binary file")
    return data


class BinaryDemo:
    def __init__(self, source):
        self.source = source
        self.size = source.seek(0, 2)
        source.seek(0)
        magic, version, size, self.count, sections, descriptor_count = struct.unpack(
            "<4sIQQII", read_exact(source, 32)
        )
        require(magic == b"PWAV" and version == 1, "Expected PWAV version 1")
        require(size == self.size, "Binary header file size mismatch")
        require(self.count > 0, "Binary contains no samples")
        require(3 <= sections <= 4096, "Invalid binary section count")
        require(descriptor_count == len(CHANNELS), "Expected twelve waveform channels")
        seen = set()
        self.fields = []
        footer = None
        sample_bytes = None
        for _ in range(sections):
            section_id, flags, length = struct.unpack("<IIQ", read_exact(source, 16))
            start = source.tell()
            end = start + length
            require(end <= self.size, "Binary section extends beyond the file")
            require(flags == 0, "Unsupported binary section flags")
            if section_id in (1, 2, 3):
                require(section_id not in seen, "Duplicate binary section")
                seen.add(section_id)
            if section_id == 1:
                require(length == descriptor_count * 24, "Descriptor section length mismatch")
                for _ in range(descriptor_count):
                    name_bytes, code, width = struct.unpack("<16sII", read_exact(source, 24))
                    name = name_bytes.rstrip(b"\0").decode("ascii")
                    require(code in FORMATS, f"Unknown numeric format code {code}")
                    fmt = "<" + FORMATS[code]
                    require(width == struct.calcsize(fmt), f"Invalid width for {name}")
                    self.fields.append((name, fmt, width))
            elif section_id == 2:
                self.sample_offset = start
                sample_bytes = length
            elif section_id == 3:
                require(length == 48, "Footer section length mismatch")
                footer = struct.unpack("<8sQQQQQ", read_exact(source, 48))
            source.seek(end)
        require(source.tell() == self.size, "Bytes remain after the declared binary sections")
        require(seen == {1, 2, 3}, "Missing descriptors, samples, or footer")
        names = [field[0] for field in self.fields]
        require(len(set(names)) == len(names) and set(names) == set(CHANNELS),
                "Binary channel names differ from the waveform demo")
        self.record_bytes = sum(field[2] for field in self.fields)
        require(self.record_bytes == 64, "Expected 64-byte waveform records")
        require(sample_bytes == self.count * self.record_bytes, "Binary sample count/length mismatch")
        require(footer == (b"PWAV-END", self.count, sample_bytes,
                           self.sample_offset, self.count - 1, 0), "Binary footer mismatch")

    def row(self, index):
        require(0 <= index < self.count, f"Sample {index} is outside the binary data")
        self.source.seek(self.sample_offset + index * self.record_bytes)
        record = read_exact(self.source, self.record_bytes)
        offset = 0
        values = {}
        for name, fmt, width in self.fields:
            values[name] = struct.unpack_from(fmt, record, offset)[0]
            offset += width
        return values


def read_line(source):
    line = source.readline(MAX_LINE_BYTES + 1)
    require(len(line) <= MAX_LINE_BYTES, "CSV row exceeds the verification line limit")
    require(not line or line.endswith(b"\n"), "CSV ends with an incomplete row")
    return line


def csv_row(line):
    try:
        row = next(csv.reader([line.decode("ascii")]))
        require(len(row) == len(CHANNELS) + 1, "CSV column count mismatch")
        index = int(row[0])
        require(index >= 0, "Negative CSV sample index")
        values = dict(zip(CHANNELS, map(float, row[1:])))
        require(all(math.isfinite(value) for value in values.values()), "Non-finite CSV value")
        return index, values
    except (UnicodeError, csv.Error, StopIteration) as error:
        raise ValueError(f"Invalid CSV row: {error}") from error


def verify(binary_path, csv_path, expected_size=None, probe_count=64):
    require(1 <= probe_count <= 4096, "Probe count must be between 1 and 4096")
    with Path(binary_path).open("rb") as binary_source, Path(csv_path).open("rb") as csv_source:
        binary = BinaryDemo(binary_source)
        csv_size = csv_source.seek(0, 2)
        if expected_size is not None:
            require(binary.size == expected_size, "Binary does not have the expected byte size")
            require(csv_size == expected_size, "CSV does not have the expected byte size")
        csv_source.seek(0)
        header = next(csv.reader([read_line(csv_source).decode("ascii")]))
        require(header == ["time", *CHANNELS], "CSV header does not match the waveform demo")
        data_start = csv_source.tell()
        period = [binary.row(index) for index in range(min(PERIOD, binary.count))]
        checked_csv = set()
        checked_binary = set(range(len(period)))

        def check(line):
            index, values = csv_row(line)
            actual = binary.row(index)
            require(actual == period[index % PERIOD], f"Binary waveform period mismatch at sample {index}")
            for name in CHANNELS:
                require(values[name] == actual[name],
                        f"Sample {index}, {name}: CSV {values[name]!r} != binary {actual[name]!r}")
            checked_csv.add(index)
            checked_binary.add(index)
            return index

        # Read the last complete rows, ignoring the generator's small newline padding.
        tail_start = max(data_start, csv_size - 16 * MAX_LINE_BYTES)
        csv_source.seek(tail_start)
        if tail_start > data_start:
            read_line(csv_source)
        tail_rows = []
        padding = 0
        saw_padding = False
        while line := read_line(csv_source):
            if not line.strip():
                saw_padding = True
                padding += len(line)
            else:
                require(not saw_padding, "Blank CSV padding appears before the last data row")
                tail_rows.append(line)
        require(tail_rows, "CSV contains no complete samples near its end")
        last_index = check(tail_rows[-1])
        csv_count = last_index + 1
        require(csv_count <= binary.count, "CSV has more samples than the binary fixture")
        require(padding < MAX_LINE_BYTES, "Unexpectedly large CSV newline padding")
        if len(tail_rows) > 1:
            require(check(tail_rows[-2]) + 1 == last_index, "CSV final sample indices are not consecutive")

        csv_source.seek(data_start)
        for expected_index in range(min(PERIOD, csv_count)):
            require(check(read_line(csv_source)) == expected_index,
                    "CSV initial sample indices are not consecutive")

        for probe in range(1, probe_count + 1):
            position = data_start + (csv_size - data_start) * probe // (probe_count + 1)
            csv_source.seek(position - 1)
            if csv_source.read(1) != b"\n":
                read_line(csv_source)
            previous = None
            for _ in range(3):
                line = read_line(csv_source)
                if not line or not line.strip():
                    break
                index = check(line)
                require(index <= last_index, "CSV probe index exceeds the final sample index")
                require(previous is None or index == previous + 1,
                        "CSV probe sample indices are not consecutive")
                previous = index

        # Also cover the binary-only tail when the two formats have different row counts.
        for probe in range(probe_count + 1):
            index = (binary.count - 1) * probe // probe_count
            require(binary.row(index) == period[index % PERIOD],
                    f"Binary waveform period mismatch at sample {index}")
            checked_binary.add(index)
        return {
            "binary_bytes": binary.size, "binary_samples": binary.count,
            "csv_bytes": csv_size, "csv_samples": csv_count, "csv_padding_bytes": padding,
            "sample_offset": binary.sample_offset, "channels": len(CHANNELS),
            "matched_csv_rows": len(checked_csv), "checked_binary_rows": len(checked_binary),
        }


class VerificationTests(unittest.TestCase):
    def make_pair(self, directory, reordered=False):
        binary_path = Path(directory) / "waveforms.bin"
        csv_path = Path(directory) / "waveforms.csv"
        formats = [3] * 4 + [4] * 4 + [6] * 2 + [5] * 2
        fields = list(zip(CHANNELS, formats))
        if reordered:
            fields.reverse()
        descriptor = b"".join(struct.pack("<16sII", name.encode(), code,
                                          struct.calcsize("<" + FORMATS[code])) for name, code in fields)
        section = lambda code, data: struct.pack("<IIQ", code, 0, len(data)) + data
        count = 800
        value = lambda index, name: index % PERIOD + CHANNELS.index(name)
        data = b"".join(struct.pack("<" + FORMATS[code], value(index, name))
                        for index in range(count) for name, code in fields)
        prefix = section(1, descriptor) + section(99, b"unknown payload")
        offset = 32 + len(prefix) + 16
        footer = section(3, struct.pack("<8sQQQQQ", b"PWAV-END", count, len(data), offset, count - 1, 0))
        size = offset + len(data) + len(footer)
        binary_path.write_bytes(struct.pack("<4sIQQII", b"PWAV", 1, size, count, 4, len(CHANNELS))
                                + prefix + section(2, data) + footer)
        with csv_path.open("w", newline="") as target:
            writer = csv.writer(target, lineterminator="\n")
            writer.writerow(["time", *CHANNELS])
            for index in range(600):
                writer.writerow([index, *(value(index, name) for name in CHANNELS)])
            target.write("\n" * 19)
        return binary_path, csv_path

    def test_matches_with_unknown_section_and_different_row_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            pair = self.make_pair(directory)
            result = verify(*pair)
            self.assertEqual(result["binary_samples"], 800)
            self.assertEqual(result["csv_samples"], 600)
            self.assertEqual(result["csv_padding_bytes"], 19)
            self.assertGreaterEqual(result["matched_csv_rows"], PERIOD)

    def test_discovers_reordered_descriptors(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(verify(*self.make_pair(directory, reordered=True))["channels"], 12)

    def test_rejects_corrupt_first_csv_value(self):
        with tempfile.TemporaryDirectory() as directory:
            binary_path, csv_path = self.make_pair(directory)
            text = csv_path.read_text()
            csv_path.write_text(text.replace("\n0,0,", "\n0,999,", 1))
            with self.assertRaisesRegex(ValueError, "Sample 0, sine"):
                verify(binary_path, csv_path)

    def test_rejects_nonconsecutive_final_index(self):
        with tempfile.TemporaryDirectory() as directory:
            binary_path, csv_path = self.make_pair(directory)
            lines = csv_path.read_text().splitlines(keepends=True)
            # Preserve waveform phase so value comparison alone cannot catch this.
            lines[600] = lines[600].replace("599,", "215,", 1)
            csv_path.write_text("".join(lines))
            with self.assertRaisesRegex(ValueError, "final sample indices"):
                verify(binary_path, csv_path)

    def test_rejects_corrupt_footer(self):
        with tempfile.TemporaryDirectory() as directory:
            binary_path, csv_path = self.make_pair(directory)
            data = bytearray(binary_path.read_bytes())
            data[-1] = 1
            binary_path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "footer mismatch"):
                verify(binary_path, csv_path)

    def test_rejects_incomplete_csv_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            binary_path, csv_path = self.make_pair(directory)
            csv_path.write_bytes(csv_path.read_bytes().rstrip(b"\n"))
            with self.assertRaisesRegex(ValueError, "incomplete row"):
                verify(binary_path, csv_path)

    def test_rejects_wrong_expected_size(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "expected byte size"):
                verify(*self.make_pair(directory), expected_size=2_000_000_000)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("samples/data/waveform_demo_2gb.bin"))
    parser.add_argument("--csv", type=Path, default=Path("samples/data/waveform_demo_2gb.csv"))
    parser.add_argument("--expected-size-bytes", type=int, help="require both files to have this exact size")
    parser.add_argument("--probes", type=int, default=64, help="distributed probe windows (default: 64)")
    parser.add_argument("--self-test", action="store_true", help="run small temporary-fixture regression tests")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(VerificationTests))
        return 0 if result.wasSuccessful() else 1
    try:
        result = verify(args.binary, args.csv, args.expected_size_bytes, args.probes)
    except (OSError, ValueError, struct.error, UnicodeError) as error:
        print(f"Verification failed: {error}", file=sys.stderr)
        return 1
    print(f"Binary: {result['binary_bytes']:,} bytes; {result['binary_samples']:,} samples")
    print(f"CSV:    {result['csv_bytes']:,} bytes; {result['csv_samples']:,} samples; "
          f"{result['csv_padding_bytes']} trailing newline bytes")
    print(f"Discovered {result['channels']} channels; binary samples start at byte {result['sample_offset']:,}.")
    print(f"Matched {result['matched_csv_rows']} CSV rows against binary values; "
          f"checked {result['checked_binary_rows']} binary rows against the waveform period.")
    print("Checked metadata, first period, tail, and distributed probes; not a full-file checksum or row scan.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
