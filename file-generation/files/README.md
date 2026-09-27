# RTLM demo telemetry for BinWave

Synthetic, deterministic robotics telemetry with planted anomalies at known byte offsets.

| File | Purpose |
|---|---|
| `gen_rtlm.py` | Generates the log and `<file>.manifest.json` (numpy only) |
| `rtlm_verify.py` | Reference reader; checks every planted event, `--full` decodes everything |
| `rtlm_sectioned_decoder.py` | Draft BinWave Python decoder for the sectioned layout |

```
python gen_rtlm.py                              # 6 h, interleaved, ~982 MB, ~15 s
python gen_rtlm.py --layout sectioned           # same data, contiguous sections
python gen_rtlm.py --hours 40 -o huge.rtlm      # ~6.5 GB
python rtlm_verify.py telemetry_6h_interleaved.rtlm
```

Same `--seed` + duration always gives byte-identical output.

## Format (all little-endian)

**Header, 64 bytes at 0x0**

| Off | Type | Field |
|---|---|---|
| 0 | char[4] | magic `RTLM` |
| 4 | u16 | version (1) |
| 6 | u8 | layout: 0 interleaved, 1 sectioned |
| 7 | u8 | stream_count |
| 8 | u32 | data_offset (1024) |
| 12 | u64 | start_unix_us |
| 20 | u64 | duration_us |
| 28 | u64 | file_bytes (patched at end) |
| 36 | u64 | total_records |
| 44 | u32 | seed |
| 48 | u8[16] | reserved |

**Stream descriptors, starting at 0x40**, one per stream:
24-byte `StreamDesc` (stream_id u8, field_count u8, rate_hz u16, payload_bytes u16, reserved u16, name char[16])
followed by `field_count` × 20-byte `FieldDesc` (name char[16], format u8, byte_width u8, reserved u16).
Format codes: 1 u64, 2 i64, 3 f64, 4 f32, 5 u32, 6 i32, 7 u16, 8 u8, 9 i16. Zero padding to `data_offset`.

**Record**: `sync u16 = 0xA55A` · `stream_id u8` · `seq u8` (sample index mod 256) · `t_us u64` · payload · `crc u16`
(CRC-16/CCITT-FALSE, poly 0x1021, init 0xFFFF, over stream_id through end of payload).

| id | Stream | Rate | Record | Payload |
|---|---|---|---|---|
| 1 | imu | 1000 Hz | 42 B | ax ay az gx gy gz temp_c (f32) |
| 2 | motors | 100 Hz | 30 B | rpm_0..rpm_3 (f32) |
| 3 | gps | 10 Hz | 42 B | lat lon (f64), alt_m speed_mps (f32), sats fix (u8), reserved (u16) |
| 4 | battery | 1 Hz | 30 B | voltage current_a soc_pct temp_c (f32) |

**Interleaved**: records from all streams in timestamp order from `data_offset` to EOF.
**Sectioned**: per stream, a 16-byte section header (stream_id u32, record_bytes u32, record_count u64) then fixed-stride records.

## Planted events (6 h defaults)

| Event | Where | What a viewer should see |
|---|---|---|
| imu_impact | IMU record **18,000,000** | ~16 g spike in ax/ay/az + 200-sample ring-down |
| gyro_stuck | IMU at 55% | gx flatlines for 2 s |
| imu_logging_gap | IMU at 90% | 250 ms missing; t_us and seq jump |
| motor3_stall | motors at 41% | rpm_3 → 0 for ~1 s, others +8% |
| gps_dropout | GPS at 27% | fix=0, sats 0-3, position frozen 45 s |
| battery_sag | battery at 72% | −2.8 V, +22 A for 3 s |
| bad_crc ×7 | random IMU/motors | CRC wrong, payload intact |
| garbage_burst | interleaved only, ~50% | 37 junk bytes; decoder must resync |
| truncated_final_record | interleaved only, EOF | last 19 bytes of a 42-byte record |

Exact offsets, timestamps and record indices are in the manifest. Files shorter than ~5.3 h put the impact at 83% instead of record 18M.
