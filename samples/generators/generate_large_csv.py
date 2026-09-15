#!/usr/bin/env python3
"""
Generate a large CSV file to stress-test BinWave.

Defaults create ~20 GiB at samples/data/large_test.csv with a time column plus N signals.
Example: python samples/generators/generate_large_csv.py --gb 20 --signals 8 --out samples/data/large_test.csv
"""

import argparse
import math
import os
from pathlib import Path
from typing import List


def make_row(row_idx: int, signals: int) -> str:
    # Keep computation cheap and deterministic.
    time_val = row_idx
    vals: List[str] = []
    for s in range(signals):
        vals.append(str((row_idx * (s + 1)) % 10_000))
    return f"{time_val}," + ",".join(vals) + "\n"


def make_header(signals: int) -> str:
    cols = ["time"] + [f"signal_{i+1}" for i in range(signals)]
    return ",".join(cols) + "\n"


def estimate_line_len(signals: int) -> int:
    return len(make_row(0, signals))


def generate(out_path: Path, target_gb: float, signals: int, chunk_rows: int) -> None:
    target_bytes = int(target_gb * 1024**3)
    line_len = estimate_line_len(signals)
    total_rows = math.ceil(target_bytes / line_len)

    out_path.parent.mkdir(parents=True, exist_ok=True)
    written = 0
    row_idx = 0
    with open(out_path, "w", encoding="utf-8") as f:
        header = make_header(signals)
        f.write(header)
        written += len(header)
        while written < target_bytes:
            rows = min(chunk_rows, total_rows - row_idx)
            chunk = [make_row(row_idx + i, signals) for i in range(rows)]
            buf = "".join(chunk)
            f.write(buf)
            written += len(buf)
            row_idx += rows
    print(
        f"Wrote {written / 1024**3:.2f} GiB to {out_path} "
        f"({row_idx} rows, ~{signals} signals)."
    )


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate a large CSV for BinWave.")
    parser.add_argument(
        "--gb",
        type=float,
        default=20.0,
        help="Approximate size to generate in GiB (default: 20).",
    )
    parser.add_argument(
        "--signals",
        type=int,
        default=8,
        help="Number of signal columns (default: 8).",
    )
    parser.add_argument(
        "--chunk-rows",
        type=int,
        default=200_000,
        help="Rows to buffer per write (larger = faster, more RAM).",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "data" / "large_test.csv",
        help="Output CSV path (default: samples/data/large_test.csv).",
    )
    args = parser.parse_args()
    generate(args.out, args.gb, args.signals, args.chunk_rows)


if __name__ == "__main__":
    main()
