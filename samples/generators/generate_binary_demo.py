#!/usr/bin/env python3
"""Regenerate sensors.bin exactly; record 73 has the temperature spike."""
from pathlib import Path
import struct

out = bytearray(b'BINWAVE-DEMO\x00')  # 16-byte header
for index in range(200):
    # Deliberately duplicated and unsorted timestamps around the spike.
    timestamp = 1700000000 + (70 if index in (72, 73) else index)
    temperature = 8500 if index == 73 else 2200 + (index % 21 - 10) * 7
    pressure = 1010 + (index % 13 - 6)
    out += struct.pack('<IhH', timestamp, temperature, pressure)
    out += bytes((0xA5, index % 256, 0x5A, 0xFF))  # preserved padding
# Literal marker followed by an ordinary illustrative checksum value.
# BinWave does not verify or recalculate this field.
out += bytes.fromhex('DE AD BE EF') + struct.pack('<I', 0x12345678)
path = Path(__file__).resolve().parents[1] / 'data' / 'sensors.bin'
path.parent.mkdir(parents=True, exist_ok=True)
path.write_bytes(out)
print(f'{path}: {len(out)} bytes; spike record 73 at 0x{16 + 73 * 12:X}')
