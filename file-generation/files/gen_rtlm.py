#!/usr/bin/env python3
"""
gen_rtlm.py - Synthetic multi-rate robotics telemetry log ("RTLM") for BinWave demos.

Writes a self-describing binary log with four streams at different rates
(IMU 1 kHz, motors 100 Hz, GPS 10 Hz, battery 1 Hz), plants known anomalies
at known byte offsets, and writes a JSON manifest of every planted event.

Deterministic: the same --seed and --hours always produce identical bytes.
Streams to disk in 10-second chunks, so memory stays flat at any file size.

Layouts
  interleaved  Records from all streams merged in timestamp order, like real
               firmware loggers. Includes a garbage burst (needs resync) and a
               truncated final record. This is the realistic, harder case.
  sectioned    Each stream in its own length-prefixed section of fixed-stride
               records. Easy to decode with file.records().

Usage
  python gen_rtlm.py                         # 6 h, interleaved, ~1 GB
  python gen_rtlm.py --hours 12 --layout sectioned -o big.rtlm
  python gen_rtlm.py --minutes 5 -o small.rtlm   # quick test file

Requires: numpy
"""
import argparse
import json
import math
import os
import shutil
import struct
import sys
import tempfile
import time

import numpy as np

MAGIC = b"RTLM"
VERSION = 1
SYNC = 0xA55A                 # stored little-endian: bytes 5A A5
LAYOUTS = {"interleaved": 0, "sectioned": 1}
DATA_ALIGN = 1024             # records start at the first 1 KiB boundary after descriptors
CHUNK_S = 10                  # seconds generated per write

# Format codes match the PWAV demo so decoders can share a lookup table.
FMT_CODE = {"u64": 1, "i64": 2, "f64": 3, "f32": 4, "u32": 5, "i32": 6, "u16": 7, "u8": 8, "i16": 9}
NP_TYPE = {"u64": "<u8", "i64": "<i8", "f64": "<f8", "f32": "<f4", "u32": "<u4",
           "i32": "<i4", "u16": "<u2", "u8": "u1", "i16": "<i2"}

STREAMS = [
    dict(id=1, name="imu", rate=1000, jitter_us=3,
         fields=[("ax", "f32"), ("ay", "f32"), ("az", "f32"),
                 ("gx", "f32"), ("gy", "f32"), ("gz", "f32"), ("temp_c", "f32")]),
    dict(id=2, name="motors", rate=100, jitter_us=40,
         fields=[("rpm_0", "f32"), ("rpm_1", "f32"), ("rpm_2", "f32"), ("rpm_3", "f32")]),
    dict(id=3, name="gps", rate=10, jitter_us=400,
         fields=[("lat", "f64"), ("lon", "f64"), ("alt_m", "f32"), ("speed_mps", "f32"),
                 ("sats", "u8"), ("fix", "u8"), ("reserved", "u16")]),
    dict(id=4, name="battery", rate=1, jitter_us=900,
         fields=[("voltage", "f32"), ("current_a", "f32"), ("soc_pct", "f32"), ("temp_c", "f32")]),
]
BY_ID = {s["id"]: s for s in STREAMS}
BY_NAME = {s["name"]: s for s in STREAMS}

for s in STREAMS:
    s["dtype"] = np.dtype(
        [("sync", "<u2"), ("stream_id", "u1"), ("seq", "u1"), ("t_us", "<u8")]
        + [(n, NP_TYPE[t]) for n, t in s["fields"]]
        + [("crc", "<u2")]
    )
    s["record_bytes"] = s["dtype"].itemsize

HEADER_FMT = "<4sHBBIQQQQI16s"   # 64 bytes
STREAM_DESC_FMT = "<BBHHH16s"    # 24 bytes
FIELD_DESC_FMT = "<16sBBH"       # 20 bytes
SECTION_FMT = "<IIQ"             # 16 bytes
assert struct.calcsize(HEADER_FMT) == 64


# ---------------------------------------------------------------- CRC-16/CCITT-FALSE
def _crc_table():
    t = np.zeros(256, np.uint32)
    for i in range(256):
        c = i << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) if c & 0x8000 else (c << 1)
        t[i] = c & 0xFFFF
    return t


CRC_TABLE = _crc_table()


def crc16_rows(rows):
    """CRC-16/CCITT-FALSE of each row of a (n, k) uint8 array, vectorized over rows."""
    crc = np.full(rows.shape[0], 0xFFFF, np.uint32)
    for j in range(rows.shape[1]):
        crc = ((crc << 8) & 0xFFFF) ^ CRC_TABLE[((crc >> 8) ^ rows[:, j]) & 0xFF]
    return crc.astype(np.uint16)


def crc16(data: bytes) -> int:
    return int(crc16_rows(np.frombuffer(data, np.uint8)[None, :])[0])


assert crc16(b"123456789") == 0x29B1  # standard check value


# ---------------------------------------------------------------- anomaly plan
def make_plan(duration_s, seed):
    n_imu = duration_s * 1000
    rng = np.random.default_rng([seed, 999_999])
    impact = 18_000_000 if n_imu >= 19_000_000 else int(n_imu * 0.83)
    plan = {
        "imu_impact": impact,                                           # sample index
        "gyro_stuck": (int(0.55 * n_imu), 2000),                        # start, length (2 s)
        "imu_gap": (int(0.90 * n_imu), 250),                            # dropped samples
        "motor3_stall": (int(0.41 * duration_s * 100), 150),            # 1.5 s
        "gps_dropout": (int(0.27 * duration_s * 10), min(450, max(50, duration_s))),
        "battery_sag": (int(0.72 * duration_s), 3),
        "garbage_after_s": int(0.50 * duration_s) // CHUNK_S * CHUNK_S + CHUNK_S,
    }
    # Bad CRCs on a handful of ordinary records, away from the other events.
    lo, hi = 5_000, int(0.80 * n_imu)
    plan["bad_crc"] = [("imu", int(i)) for i in np.sort(rng.integers(lo, hi, 5))]
    plan["bad_crc"] += [("motors", int(i)) for i in np.sort(rng.integers(lo // 10, hi // 10, 2))]
    return plan


def in_range(idx, start_len):
    s, n = start_len
    return (idx >= s) & (idx < s + n)


# ---------------------------------------------------------------- signal models
TAU = 2 * math.pi


def gen_imu(idx, rng, plan, duration_s):
    idx = idx[~in_range(idx, plan["imu_gap"])]           # logging gap: samples never written
    t = idx / 1000.0
    n = len(idx)
    nz = lambda s: s * rng.standard_normal(n)
    vib = 0.35 * np.sin(TAU * 86.7 * t) + 0.12 * np.sin(TAU * 173.4 * t + 0.3)
    c = {
        "ax": 0.8 * np.sin(TAU * t / 47 + 0.5) + vib + nz(0.05),
        "ay": 0.6 * np.sin(TAU * t / 61 + 1.5) + 0.8 * vib + nz(0.05),
        "az": -9.81 + 0.25 * np.sin(TAU * t / 33) + 1.2 * vib + nz(0.05),
        "gx": 0.30 * np.sin(TAU * t / 47) + nz(0.02),
        "gy": 0.25 * np.sin(TAU * t / 61 + 1.0) + nz(0.02),
        "gz": 0.15 * np.sin(TAU * t / 120 + 2.0) + nz(0.02),
        "temp_c": 35 + 10 * (1 - np.exp(-t / 1800)) + nz(0.05),
    }
    # Impact: one huge sample, then a ~200-sample damped ring-down.
    k = idx - plan["imu_impact"]
    ring = (k >= 0) & (k < 200)
    if ring.any():
        kk = k[ring]
        env = np.exp(-kk / 30.0) * np.sin(TAU * kk / 9.0)
        c["ax"][ring] += 40 * env
        c["az"][ring] += 25 * env
        c["gy"][ring] += 3.5 * env
        hit = k == 0
        c["ax"][hit] += 147.0
        c["ay"][hit] -= 62.0
        c["az"][hit] += 95.0
    # Stuck gyro: gx freezes at its last good reading.
    stuck = in_range(idx, plan["gyro_stuck"])
    if stuck.any():
        s0 = plan["gyro_stuck"][0] / 1000.0
        c["gx"][stuck] = 0.30 * math.sin(TAU * s0 / 47)
    return idx, c


def gen_motors(idx, rng, plan, duration_s):
    t = idx / 100.0
    n = len(idx)
    base = 5200 + 300 * np.sin(TAU * t / 47) + 150 * np.sin(TAU * t / 13)
    c = {f"rpm_{i}": base + off + 15 * rng.standard_normal(n)
         for i, off in enumerate((0, 35, -20, 10))}
    s0, ln = plan["motor3_stall"]
    k = idx - s0
    m = (k >= 0) & (k < ln)
    if m.any():
        kk = k[m].astype(float)
        f = np.where(kk < 20, 1 - kk / 20, np.where(kk >= ln - 30, (kk - (ln - 30)) / 30, 0.0))
        c["rpm_3"][m] *= f
        for i in range(3):
            c[f"rpm_{i}"][m] *= 1.08   # the others compensate
    return idx, c


LAT0, LON0, R_M = 40.0150, -105.2705, 150.0


def _gps_pos(t):
    ang = TAU * t / 300
    lat = LAT0 + R_M / 111_320 * np.sin(ang)
    lon = LON0 + R_M / (111_320 * math.cos(math.radians(LAT0))) * np.cos(ang)
    alt = 1655 + 5 * np.sin(TAU * t / 90)
    return lat, lon, alt


def gen_gps(idx, rng, plan, duration_s):
    t = idx / 10.0
    n = len(idx)
    lat, lon, alt = _gps_pos(t)
    c = {
        "lat": lat + 2e-7 * rng.standard_normal(n),
        "lon": lon + 2e-7 * rng.standard_normal(n),
        "alt_m": alt + 0.3 * rng.standard_normal(n),
        "speed_mps": TAU * R_M / 300 + 0.1 * rng.standard_normal(n),
        "sats": 14 + rng.integers(-2, 3, n),
        "fix": np.full(n, 3),
        "reserved": np.zeros(n),
    }
    d = in_range(idx, plan["gps_dropout"])
    if d.any():
        flat, flon, falt = _gps_pos(np.array([plan["gps_dropout"][0] / 10.0]))
        c["lat"][d], c["lon"][d], c["alt_m"][d] = flat[0], flon[0], falt[0]
        c["speed_mps"][d] = 0.0
        c["fix"][d] = 0
        c["sats"][d] = rng.integers(0, 4, int(d.sum()))
    return idx, c


def gen_battery(idx, rng, plan, duration_s):
    t = idx.astype(float)
    n = len(idx)
    frac = t / duration_s
    c = {
        "voltage": 25.2 - 3.6 * frac - 0.25 * np.sin(TAU * t / 47) ** 2 + 0.01 * rng.standard_normal(n),
        "current_a": 18 + 3 * np.sin(TAU * t / 47) + 0.2 * rng.standard_normal(n),
        "soc_pct": 100 - 82 * frac,
        "temp_c": 28 + 9 * frac + 0.1 * rng.standard_normal(n),
    }
    sag = in_range(idx, plan["battery_sag"])
    c["voltage"][sag] -= 2.8
    c["current_a"][sag] += 22
    return idx, c


GENERATORS = {"imu": gen_imu, "motors": gen_motors, "gps": gen_gps, "battery": gen_battery}


def build_records(stream, idx, cols, rng, plan):
    a = np.zeros(len(idx), stream["dtype"])
    a["sync"] = SYNC
    a["stream_id"] = stream["id"]
    a["seq"] = idx & 0xFF                               # gaps show up as seq jumps
    period = 1_000_000 // stream["rate"]
    j = stream["jitter_us"]
    a["t_us"] = idx * period + rng.integers(-j, j + 1, len(idx))
    for k, v in cols.items():
        a[k] = v
    raw = a.view(np.uint8).reshape(len(a), -1)
    a["crc"] = crc16_rows(raw[:, 2:-2])                 # CRC over stream_id .. end of payload
    bad = [i for name, i in plan["bad_crc"] if name == stream["name"]]
    if bad:
        m = np.isin(idx, bad)
        a["crc"][m] ^= 0xBEEF
    return a


# ---------------------------------------------------------------- file structure
def descriptor_bytes():
    out = b""
    for s in STREAMS:
        out += struct.pack(STREAM_DESC_FMT, s["id"], len(s["fields"]), s["rate"],
                           s["record_bytes"] - 14, 0, s["name"].encode().ljust(16, b"\0"))
        for n, t in s["fields"]:
            out += struct.pack(FIELD_DESC_FMT, n.encode().ljust(16, b"\0"),
                               FMT_CODE[t], np.dtype(NP_TYPE[t]).itemsize, 0)
    return out


def header_bytes(layout, duration_s, seed, file_bytes=0, total_records=0, start_unix_us=0):
    return struct.pack(HEADER_FMT, MAGIC, VERSION, LAYOUTS[layout], len(STREAMS),
                       DATA_ALIGN, start_unix_us, duration_s * 1_000_000,
                       file_bytes, total_records, seed, b"\0" * 16)


def generate(path, duration_s, layout, seed, start_unix_us):
    plan = make_plan(duration_s, seed)
    desc = descriptor_bytes()
    assert 64 + len(desc) <= DATA_ALIGN

    # Probes: (stream, sample_index) whose byte offset the manifest needs.
    probes = {("imu", plan["imu_impact"]): None,
              ("imu", plan["gyro_stuck"][0]): None,
              ("imu", plan["imu_gap"][0] - 1): None,
              ("imu", plan["imu_gap"][0] + plan["imu_gap"][1]): None,
              ("motors", plan["motor3_stall"][0]): None,
              ("gps", plan["gps_dropout"][0]): None,
              ("battery", plan["battery_sag"][0]): None}
    for p in plan["bad_crc"]:
        probes[p] = None
    probe_info = {}                         # (stream, idx) -> (record_index, t_us, local offset)

    tmpdir = tempfile.mkdtemp(prefix="rtlm_")
    counts = {s["name"]: 0 for s in STREAMS}
    extra = []                              # manifest entries for garbage / truncation
    t0 = time.time()
    n_chunks = math.ceil(duration_s / CHUNK_S)

    with open(path, "wb") as f:
        f.write(header_bytes(layout, duration_s, seed))
        f.write(desc)
        f.write(b"\0" * (DATA_ALIGN - f.tell()))
        section_files = {}
        if layout == "sectioned":
            section_files = {s["name"]: open(os.path.join(tmpdir, s["name"] + ".bin"), "wb")
                             for s in STREAMS}

        for c in range(n_chunks):
            s0, s1 = c * CHUNK_S, min(duration_s, (c + 1) * CHUNK_S)
            rng = np.random.default_rng([seed, c])
            arrays = []
            for s in STREAMS:
                idx = np.arange(s0 * s["rate"], s1 * s["rate"], dtype=np.int64)
                idx, cols = GENERATORS[s["name"]](idx, rng, plan, duration_s)
                recs = build_records(s, idx, cols, rng, plan)
                arrays.append((s, idx, recs))

            if layout == "interleaved":
                ts = np.concatenate([r["t_us"] for _, _, r in arrays])
                sid = np.concatenate([r["stream_id"] for _, _, r in arrays])
                sizes = np.concatenate([np.full(len(r), r.dtype.itemsize, np.int64)
                                        for _, _, r in arrays])
                order = np.lexsort((sid, ts))
                ss = sizes[order]
                starts_sorted = np.concatenate(([0], np.cumsum(ss)[:-1]))
                starts = np.empty_like(starts_sorted)
                starts[order] = starts_sorted
                buf = np.empty(int(ss.sum()), np.uint8)
                base = f.tell()
                p = 0
                for s, idx, r in arrays:
                    raw = r.view(np.uint8).reshape(len(r), -1)
                    st = starts[p:p + len(r)]
                    buf[st[:, None] + np.arange(raw.shape[1])] = raw
                    for (name, pi) in probes:
                        if name == s["name"]:
                            j = np.searchsorted(idx, pi)
                            if j < len(idx) and idx[j] == pi:
                                probe_info[(name, pi)] = (counts[name] + int(j),
                                                          int(r["t_us"][j]), base + int(st[j]))
                    counts[s["name"]] += len(r)
                    p += len(r)
                f.write(buf.tobytes())

                if s1 == plan["garbage_after_s"]:
                    junk = np.random.default_rng([seed, 424242]).integers(0, 256, 37, np.uint8)
                    extra.append(dict(name="garbage_burst", byte_offset=f.tell(), length=37,
                                      description="37 random bytes between records; a decoder "
                                                  "must resync on the sync word and a valid CRC"))
                    f.write(junk.tobytes())
            else:
                for s, idx, r in arrays:
                    for (name, pi) in probes:
                        if name == s["name"]:
                            j = np.searchsorted(idx, pi)
                            if j < len(idx) and idx[j] == pi:
                                probe_info[(name, pi)] = (counts[name] + int(j),
                                                          int(r["t_us"][j]), None)
                    counts[s["name"]] += len(r)
                    section_files[s["name"]].write(r.tobytes())

            if c % 60 == 0 or c == n_chunks - 1:
                done = (c + 1) / n_chunks
                written = sum(counts[s["name"]] * s["record_bytes"] for s in STREAMS)
                print(f"\r  {done:6.1%}  {written / 1e6:9.1f} MB  {time.time() - t0:6.1f}s",
                      end="", file=sys.stderr, flush=True)

        # Finish the file.
        section_starts = {}
        if layout == "interleaved":
            s = BY_NAME["imu"]
            nxt = duration_s * 1000
            rng = np.random.default_rng([seed, 777])
            idx, cols = gen_imu(np.array([nxt]), rng, plan, duration_s)
            tail = build_records(s, idx, cols, rng, plan).tobytes()[:19]
            extra.append(dict(name="truncated_final_record", byte_offset=f.tell(), length=19,
                              description=f"first 19 of {s['record_bytes']} bytes of an IMU "
                                          "record; logger lost power mid-write"))
            f.write(tail)
        else:
            for s in STREAMS:
                section_files[s["name"]].close()
                f.write(struct.pack(SECTION_FMT, s["id"], s["record_bytes"], counts[s["name"]]))
                section_starts[s["name"]] = f.tell()
                with open(os.path.join(tmpdir, s["name"] + ".bin"), "rb") as src:
                    shutil.copyfileobj(src, f, 16 * 1024 * 1024)

        file_bytes = f.tell()
        total = sum(counts.values())
        f.seek(0)
        f.write(header_bytes(layout, duration_s, seed, file_bytes, total, start_unix_us))
    shutil.rmtree(tmpdir, ignore_errors=True)
    print(file=sys.stderr)

    # Resolve sectioned offsets now that section positions are known.
    def offset(name, pi):
        rec_i, t_us, off = probe_info[(name, pi)]
        if off is None:
            off = section_starts[name] + rec_i * BY_NAME[name]["record_bytes"]
        return rec_i, t_us, off

    def entry(name, stream, pi, desc_, **kw):
        rec_i, t_us, off = offset(stream, pi)
        return dict(name=name, stream=stream, sample_index=pi, record_index=rec_i,
                    t_us=t_us, byte_offset=off, description=desc_, **kw)

    g0, gl = plan["imu_gap"]
    anomalies = [
        entry("imu_impact", "imu", plan["imu_impact"],
              "single-sample shock (ax +147, ay -62, az +95 m/s^2) then 200-sample damped ring-down",
              fields=["ax", "ay", "az", "gy"]),
        entry("gyro_stuck", "imu", plan["gyro_stuck"][0],
              f"gx frozen at its last good value for {plan['gyro_stuck'][1]} samples",
              length_samples=plan["gyro_stuck"][1], fields=["gx"]),
        entry("imu_logging_gap", "imu", g0 - 1,
              f"{gl} IMU samples never written; t_us jumps ~{gl} ms and seq jumps by {gl + 1} mod 256. "
              "byte_offset is the last record before the gap",
              resume_byte_offset=offset("imu", g0 + gl)[2], dropped_samples=gl, fields=["t_us", "seq"]),
        entry("motor3_stall", "motors", plan["motor3_stall"][0],
              "rpm_3 ramps to 0, holds, recovers over 1.5 s; rpm_0..2 rise 8% to compensate",
              length_samples=plan["motor3_stall"][1], fields=["rpm_0", "rpm_1", "rpm_2", "rpm_3"]),
        entry("gps_dropout", "gps", plan["gps_dropout"][0],
              "fix=0, sats 0-3, position frozen, speed 0",
              length_samples=plan["gps_dropout"][1], fields=["fix", "sats", "lat", "lon", "speed_mps"]),
        entry("battery_sag", "battery", plan["battery_sag"][0],
              "voltage drops 2.8 V and current rises 22 A for 3 s",
              length_samples=plan["battery_sag"][1], fields=["voltage", "current_a"]),
    ]
    anomalies += [entry("bad_crc", n, i, "crc field XOR 0xBEEF; payload is intact")
                  for n, i in plan["bad_crc"]]
    anomalies += extra
    anomalies.sort(key=lambda a: a["byte_offset"])

    manifest = dict(
        file=os.path.basename(path), format="RTLM", version=VERSION, layout=layout,
        seed=seed, duration_s=duration_s, file_bytes=file_bytes, total_records=total,
        data_offset=DATA_ALIGN,
        record_header="sync u16 (0xA55A) | stream_id u8 | seq u8 | t_us u64 | payload | "
                      "crc u16 (CRC-16/CCITT-FALSE over stream_id..payload)",
        streams=[dict(id=s["id"], name=s["name"], rate_hz=s["rate"], record_bytes=s["record_bytes"],
                      record_count=counts[s["name"]],
                      section_records_offset=section_starts.get(s["name"]),
                      fields=[dict(name=n, type=t) for n, t in s["fields"]]) for s in STREAMS],
        anomalies=anomalies,
    )
    with open(path + ".manifest.json", "w") as mf:
        json.dump(manifest, mf, indent=2)
    return manifest


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--hours", type=float, help="log duration in hours (default 6)")
    g.add_argument("--minutes", type=float, help="log duration in minutes")
    ap.add_argument("--layout", choices=LAYOUTS, default="interleaved")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--start-unix-us", type=int, default=1_780_000_000_000_000,
                    help="wall-clock start stored in the header (default mid-2026)")
    ap.add_argument("-o", "--output", default=None)
    a = ap.parse_args()

    duration_s = int(round(a.minutes * 60 if a.minutes else (a.hours or 6) * 3600))
    if duration_s < 300:
        ap.error("duration must be at least 5 minutes so every anomaly fits")
    out = a.output or f"telemetry_{duration_s // 3600}h_{a.layout}.rtlm"
    est = duration_s * sum(s["rate"] * s["record_bytes"] for s in STREAMS)
    print(f"Writing {out}: {duration_s} s, {a.layout}, seed {a.seed}, ~{est / 1e9:.2f} GB",
          file=sys.stderr)
    m = generate(out, duration_s, a.layout, a.seed, a.start_unix_us)
    print(f"Done: {m['file_bytes']:,} bytes, {m['total_records']:,} records, "
          f"{len(m['anomalies'])} planted events -> {out}.manifest.json", file=sys.stderr)


if __name__ == "__main__":
    main()
