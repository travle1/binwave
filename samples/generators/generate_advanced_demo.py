"""Generate the small public multi-collection demo; no external data required."""
from pathlib import Path
import math
import struct
root = Path(__file__).resolve().parents[1]
data = bytearray(struct.pack('<4sIII', b'BWAV', 384, 192, 96))
for i in range(384):
    data += struct.pack('<dfhBB', math.sin(2*math.pi*i/64),
                        2*abs(2*((i/48)%1)-1)-1, i%96-48,
                        int(i%32<8), (i^(i>>1)) & 255)
for i in range(192):
    data += struct.pack('<ff', 20+5*math.sin(2*math.pi*i/96), 100+10*math.cos(2*math.pi*i/48))
for i in range(96):
    data += struct.pack('<HH', i%8, 1+i%16)
(root/'quickstart'/'advanced_wave.bin').write_bytes(data)
print(f'{len(data)} bytes: 384 waveforms, 192 measurements, 96 events')
