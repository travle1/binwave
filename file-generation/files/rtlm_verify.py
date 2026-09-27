#!/usr/bin/env python3
"""
rtlm_verify.py - Reference reader for RTLM files and regression check against the manifest.

Reads the layout from the file's own descriptors (nothing hardcoded), then:
  default  checks every planted event in <file>.manifest.json at its byte offset
  --full   also decodes every record and confirms the totals BinWave should find
           (sectioned: fast, vectorized; interleaved: pure-Python resync scan,
            fine up to ~10 min files, too slow for multi-GB ones)

Usage
  python rtlm_verify.py telemetry_6h_interleaved.rtlm
  python rtlm_verify.py small.rtlm --full
"""
import argparse
import json
import struct
import sys

import numpy as np

from gen_rtlm import (HEADER_FMT, STREAM_DESC_FMT, FIELD_DESC_FMT, SECTION_FMT, NP_TYPE,
                      FMT_CODE, SYNC, crc16, crc16_rows)

CODE_TYPE = {v: k for k, v in FMT_CODE.items()}


def read_layout(f):
    f.seek(0)
    (magic, version, layout, n_streams, data_offset, start_us, dur_us,
     file_bytes, total, seed, _) = struct.unpack(HEADER_FMT, f.read(64))
    assert magic == b"RTLM", f"bad magic {magic!r}"
    streams = {}
    for _ in range(n_streams):
        sid, nf, rate, payload, _, name = struct.unpack(STREAM_DESC_FMT, f.read(24))
        fields = []
        for _ in range(nf):
            fname, code, width, _ = struct.unpack(FIELD_DESC_FMT, f.read(20))
            fields.append((fname.rstrip(b"\0").decode(), NP_TYPE[CODE_TYPE[code]]))
        dt = np.dtype([("sync", "<u2"), ("stream_id", "u1"), ("seq", "u1"), ("t_us", "<u8")]
                      + fields + [("crc", "<u2")])
        assert dt.itemsize == payload + 14
        streams[sid] = dict(id=sid, name=name.rstrip(b"\0").decode(), rate=rate, dtype=dt)
    hdr = dict(version=version, layout=("interleaved", "sectioned")[layout], data_offset=data_offset,
               duration_us=dur_us, file_bytes=file_bytes, total_records=total, seed=seed)
    return hdr, streams


def rec_at(f, off, streams):
    f.seek(off)
    head = f.read(12)
    if len(head) < 12:
        return None, False
    sync, sid = struct.unpack_from("<HB", head)
    if sync != SYNC or sid not in streams:
        return None, False
    dt = streams[sid]["dtype"]
    raw = head + f.read(dt.itemsize - 12)
    if len(raw) < dt.itemsize:
        return None, False
    r = np.frombuffer(raw, dt)[0]
    return r, crc16(raw[2:-2]) == int(r["crc"])


class Checker:
    def __init__(self):
        self.fail = 0

    def __call__(self, ok, msg):
        print(("  PASS  " if ok else "  FAIL  ") + msg)
        self.fail += not ok


def check_manifest(f, hdr, streams, man, size, ck):
    ck(hdr["file_bytes"] == size == man["file_bytes"], f"file_bytes {hdr['file_bytes']:,} matches size")
    ck(hdr["total_records"] == man["total_records"], f"total_records {hdr['total_records']:,}")
    for a in man["anomalies"]:
        n, off = a["name"], a["byte_offset"]
        label = f"{n:<22} @ 0x{off:X}"
        if n == "garbage_burst":
            r, ok = rec_at(f, off, streams)
            ck(not ok, f"{label}  no valid record starts here")
            r2, ok2 = rec_at(f, off + a["length"], streams)
            ck(ok2, f"{label}  valid record resumes after {a['length']} bytes")
            continue
        if n == "truncated_final_record":
            ck(size - off == a["length"], f"{label}  file ends {a['length']} bytes into a record")
            continue
        r, ok = rec_at(f, off, streams)
        if r is None:
            ck(False, f"{label}  no record header at offset")
            continue
        same = streams[int(r["stream_id"])]["name"] == a["stream"] and int(r["t_us"]) == a["t_us"]
        ck(same, f"{label}  stream {a['stream']}, t_us {a['t_us']:,}")
        if n == "bad_crc":
            ck(not ok, f"{label}  CRC fails as planted")
            continue
        ck(ok, f"{label}  CRC valid")
        if n == "imu_impact":
            ck(r["ax"] > 100 and r["az"] > 50, f"{label}  ax={r['ax']:.1f} az={r['az']:.1f}")
        elif n == "gps_dropout":
            ck(r["fix"] == 0, f"{label}  fix={r['fix']} sats={r['sats']}")
        elif n == "battery_sag":
            ck(r["current_a"] > 30, f"{label}  voltage={r['voltage']:.2f} current={r['current_a']:.1f}")
        elif n == "imu_logging_gap":
            r2, ok2 = rec_at(f, a["resume_byte_offset"], streams)
            dt = int(r2["t_us"]) - int(r["t_us"])
            dseq = (int(r2["seq"]) - int(r["seq"])) % 256
            ck(ok2 and dt >= a["dropped_samples"] * 1000,
               f"{label}  next IMU record {dt / 1000:.1f} ms later, seq +{dseq}")


def full_sectioned(path, hdr, streams, man, ck):
    mm = np.memmap(path, np.uint8, "r")
    pos = hdr["data_offset"]
    bad = 0
    for _ in streams:
        sid, rb, count = struct.unpack(SECTION_FMT, mm[pos:pos + 16].tobytes())
        pos += 16
        s = streams[sid]
        recs = mm[pos:pos + rb * count].view(s["dtype"])
        for i in range(0, count, 2_000_000):
            raw = mm[pos + i * rb: pos + min(count, i + 2_000_000) * rb].reshape(-1, rb)
            bad += int((crc16_rows(np.asarray(raw[:, 2:-2])) != recs["crc"][i:i + len(raw)]).sum())
        ck(bool((recs["sync"] == SYNC).all() and (recs["stream_id"] == sid).all()),
           f"section {s['name']:<8} {count:>12,} records, all sync/id ok")
        if s["name"] == "imu":
            peak = int(np.argmax(np.abs(recs["ax"])))
            exp = next(a for a in man["anomalies"] if a["name"] == "imu_impact")["record_index"]
            ck(peak == exp, f"largest |ax| is imu record {peak:,} (planted {exp:,})")
            gaps = int((np.diff(recs["t_us"].astype(np.int64)) > 100_000).sum())
            ck(gaps == 1, f"imu timestamp gaps > 100 ms: {gaps}")
        pos += rb * count
    exp_bad = sum(a["name"] == "bad_crc" for a in man["anomalies"])
    ck(bad == exp_bad, f"CRC failures {bad} (planted {exp_bad})")


def full_interleaved(path, hdr, streams, ck, man):
    data = open(path, "rb").read()
    pos, end = hdr["data_offset"], len(data)
    counts = {s["name"]: 0 for s in streams.values()}
    bad = resyncs = 0
    size = {sid: s["dtype"].itemsize for sid, s in streams.items()}
    sync_b = struct.pack("<H", SYNC)
    imu_ax_max, imu_ax_idx = 0.0, -1
    truncated = 0
    while pos < end:
        ok_hdr = data[pos:pos + 2] == sync_b and pos + 2 < end and data[pos + 2] in size
        if ok_hdr:
            sid = data[pos + 2]
            n = size[sid]
            if pos + n > end:
                truncated = end - pos
                break
            raw = data[pos:pos + n]
            if crc16(raw[2:-2]) == struct.unpack_from("<H", raw, n - 2)[0]:
                s = streams[sid]
                if s["name"] == "imu":
                    ax = abs(struct.unpack_from("<f", raw, 12)[0])
                    if ax > imu_ax_max:
                        imu_ax_max, imu_ax_idx = ax, counts["imu"]
                counts[s["name"]] += 1
                pos += n
                continue
            nxt = data[pos + n:pos + n + 2] == sync_b or pos + n == end
            if nxt:                                  # bad CRC, framing intact: count and skip
                bad += 1
                counts[streams[sid]["name"]] += 1
                pos += n
                continue
        resyncs += 1                                 # lost framing: find next sync word
        pos = data.find(sync_b, pos + 1)
        if pos < 0:
            break
    for name, c in counts.items():
        exp = next(s["record_count"] for s in man["streams"] if s["name"] == name)
        ck(c == exp, f"{name:<8} {c:>12,} records decoded (expected {exp:,})")
    exp_bad = sum(a["name"] == "bad_crc" for a in man["anomalies"])
    ck(bad == exp_bad, f"CRC failures {bad} (planted {exp_bad})")
    ck(resyncs == 1, f"resyncs {resyncs} (1 garbage burst planted)")
    ck(truncated == 19, f"truncated tail {truncated} bytes")
    exp = next(a for a in man["anomalies"] if a["name"] == "imu_impact")["record_index"]
    ck(imu_ax_idx == exp, f"largest |ax| is imu record {imu_ax_idx:,} (planted {exp:,})")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--full", action="store_true")
    a = ap.parse_args()
    man = json.load(open(a.file + ".manifest.json"))
    ck = Checker()
    with open(a.file, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        hdr, streams = read_layout(f)
        print(f"{a.file}: {hdr['layout']}, {size:,} bytes, "
              f"streams: {', '.join(s['name'] for s in streams.values())}")
        check_manifest(f, hdr, streams, man, size, ck)
    if a.full:
        print("Full decode:")
        if hdr["layout"] == "sectioned":
            full_sectioned(a.file, hdr, streams, man, ck)
        else:
            full_interleaved(a.file, hdr, streams, ck, man)
    print(f"{'OK' if not ck.fail else f'{ck.fail} FAILED'}")
    sys.exit(1 if ck.fail else 0)


if __name__ == "__main__":
    main()
