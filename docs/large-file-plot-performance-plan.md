# Implementation plan: interactive plotting of 100 GB decoded files

## Objective and scope

Optimize the **native application's repeated zooming and panning** over decoded files of approximately 100 GB. Make navigation depend primarily on visible detail and reusable summaries, with raw reads reserved for unindexed regions, partially covered boundaries, and close zoom levels.

First accurate overview construction is a separate workload: previously unindexed data must be examined to guarantee that peaks are retained. Improve its throughput without allowing background scanning to impair navigation.

Optimize latency, throughput, and useful reuse. Low RAM or GPU utilization alone does not establish a bottleneck, and increasing either utilization is not an acceptance criterion. Measure cache admission/hits, CPU preparation, storage waits, GPU uploads, and frame time before changing resource allocation.

**Accepted persistence policy:** provide a limited persistent decoded-data cache by default, plus a per-file **“Keep decoded data for this file”** option. Default cache entries may be evicted and later decoded again. Kept data survives restart and automatic cache eviction; it consumes additional, visible disk space as more data is decoded. This is a required capability, not a benchmark-gated decision to omit persistence. Benchmarks determine implementation choices and cache tuning, not a promised speedup factor.

This plan covers the YAML/Python-produced decoded-collection plotting path. Preserve the shared decoder and reduction semantics used by the browser. CSV and legacy binary plotting are reference implementations and regression boundaries, not targets for a wholesale rewrite.

The architecture review was performed on September 26, 2026, including existing uncommitted performance changes. It identified code-level bottlenecks, not measured 100 GB speedups. Twenty-four focused `structure_plot::tests` passed in a release test binary; no real 100 GB performance baseline was collected.

## Instructions for the implementing agent

1. Inspect the current working tree and applicable repository instructions before editing. Preserve existing user changes. Reconfirm the code paths below because implementation may have advanced since this review.
2. Implement in the phase order below. Each phase must leave the application usable and include focused verification appropriate to the change.
3. Establish measurements before promising speedups. Distinguish first scan, warm navigation, cold navigation, and reopen performance.
4. Use deterministic, public synthetic layouts for regression fixtures. Support user-provided large files through explicit paths without embedding their contents or private decoder definitions in the repository.
5. Keep decoding semantics, source validation, byte provenance, cancellation, and stale-result rejection intact.
6. Update this document's checklist and the relevant architecture/resource-budget documentation as work completes. Record measured results, hardware, fixture layout, and remaining limitations.

## Current architecture and findings to verify

`FileSource → ByteView → numeric projection or parser → reduction → PlotData → overlay → renderer`

| Area | Current behavior | Implementation entry points |
| --- | --- | --- |
| File access | Bounded positional reads; a newly allocated byte vector per read | `src/file_source.rs`, `src/byte_store.rs` |
| Numeric projection | Fixed layouts avoid full node parsing; field decode uses primitive type/endian dispatch | `NumericPlan`, `NumericField`, `project_numeric` in `src/structure.rs` |
| Grouped reads | Parallel tiles of up to 4,096 samples; 1,024-sample summary bins | `load_group_tile`, `read_group_buffered` in `src/structure_plot.rs` |
| Cache lookup | Linear `VecDeque` searches and duplicate checks under a shared mutex | `GroupTileCache` |
| Query reuse | Flat summaries; three exact completed request entries; no decoded summary sidecar | `PlotReader`, `cached_group_summary` |
| Execution routing | Ordinary numeric plans can use sequential `read_numeric` instead of the grouped tile engine | `read_plot`; scheduling in `src/structure_views.rs` |
| Viewport changes | Changed ranges cancel requests, clear current results, and invalidate overlay/render caches | `request_with_updates`; native plot composition |
| Memory | Raw tile budget per reader, plus a separate summary allowance up to four times that budget | `group_buffer_bytes`, `GroupTileCache::insert` |
| Rendering | GPU traces at 64 visible series; eligible views with 16–63 series can rasterize synchronously | `src/structure_views.rs`, `src/plot_raster.rs`, `src/plot_gpu.rs` |
| Provenance | Samples and envelope endpoints carry path strings; overlays clone sample structures | `PlotSample`, `NumericReduction::data`, overlay assembly |

For scale, 100 GB of 64-byte records yields about 381,000 tiles at 4,096 samples per tile. If summaries remain resident, linear miss/duplicate checks can accumulate quadratic work during cache construction. Increasing cache capacity alone does not solve this.

## Required invariants

- Accurate reductions retain extrema and their actual sample indices and byte provenance. Preserve deterministic ordering and tie behavior.
- Exact views retain all required samples within the established output budget.
- Sampled previews remain explicitly identified as estimates that can miss peaks. Do not present unread coverage as fully indexed.
- Preserve existing handling of invalid values and gaps; do not introduce connections across unread intervals.
- A result from an obsolete source, decoder, or request must not replace a current result.
- Previously displayed data may remain visible during navigation only while its source and decoder identity remain valid. An edit or definition change is not merely a viewport change.
- Reads, retained memory, in-flight work, and display output have explicit bounds.
- Layout optimizations must be proven from metadata, not inferred from a sampled record or a format name.
- New native optimizations must not introduce a second decoder or break browser builds.

## Phase 0 — Establish a reproducible baseline

### Tasks

- Add a headless benchmark entry point that exercises the same native numeric query/cache engine used by the UI. Explicitly select exact/min-max scanning versus sampled preview.
- Cover both ordinary fixed collections and segmented/grouped collections. Include narrow and wide records, mixed numeric types/endian settings, and one/many selected fields.
- Include variable-size collections with verified record boundaries and, where supported, state-dependent decoding. Exercise close-range revisits after RAM eviction as well as after process restart.
- Add low-overhead counters and timings for source bytes/read calls, decoded samples, raw/summary cache hits and misses, cache lock wait, reduction, published output, cancellation, and memory categories. Collect at batch boundaries rather than adding expensive instrumentation per sample.
- Distinguish RAM decoded-tile hits, disk decoded-chunk hits, summary hits, and source re-decoding. For persistent caches, record bytes read/written, serialization/compression cost, and disk footprint separately from source I/O.
- Measure native frame time and gesture-to-result latency separately from loader/query time. Existing slow-phase warnings at 200 ms are too coarse for interactive frame budgets.
- Provide a deterministic navigation trace: initial overview, wide pan, repeated zoom-in/out, short adjacent pans, return to a previous region, channel selection changes, and reopen.
- Include a real allocated-file workload. Sparse-file fixtures remain useful for address-range correctness but do not establish storage throughput.

### Acceptance

- Record baseline p50/p95 query latency, gesture-to-update latency, frame time, source bytes read, cancelled work, and peak memory for the navigation trace.
- Document CPU, RAM, storage/filesystem, build flags, file size, record layout, channel count, display resolution, and cache conditions.
- Label OS-cache state honestly; an application-cache miss is not necessarily a cold disk read. Avoid system-wide cache eviction as a routine benchmark step.
- Agree on hardware-specific performance gates from the baseline. Initial aspirational targets are p95 warm navigation refinement below 100 ms and p95 frame time below 16.7 ms at 60 Hz; these are targets, not current guarantees.

## Phase 1 — Eliminate linear cache operations

### Tasks

- Replace `VecDeque` searches with direct tile-number indexing or a hash map. Maintain eviction ordering separately with bounded, constant-time updates; a map pointing into a shifting vector is insufficient.
- Apply the same change to summary lookup and duplicate detection.
- Keep file reads and decoding outside the cache lock. Batch lookups/publication where practical and measure lock contention.
- Represent concurrent requests for the same tile as shared in-flight work where useful, with safe completion/error cleanup.
- Retain accounting, eviction, revision separation, and cancellation behavior.

### Acceptance

- Test hits, misses, replacement, eviction, duplicate concurrent loads, cancellation, and source/decoder invalidation.
- Benchmark cache metadata operations at increasing cardinalities, including hundreds of thousands of entries. Lookup and insertion must not scan all retained entries.
- Show baseline-versus-new cache timings and unchanged numerical output.

## Phase 2 — Decouple the viewport from data requests

### Tasks

- Separate the exact display viewport from the aligned loaded/query coverage and its resolution level.
- Query aligned tiles with modest, bounded coverage outside the viewport. Use measured pan direction to guide optional prefetch.
- Keep the last valid plot visible while navigating; transform/clip existing geometry immediately. Clearly represent pending or incomplete coverage.
- Coalesce gesture-driven requests and refine at a bounded cadence or after a short settle interval. Avoid an unconditional long delay on every interaction.
- Cancel obsolete viewport work while retaining completed useful tiles. Ensure only the latest applicable result is published.
- Stop invalidating geometry merely because display bounds changed when existing geometry still has adequate coverage and resolution. Include actual data identity, resolution, visibility, and presentation state in the relevant cache keys.
- Audit worker/prefetch ownership: the existing worker marks itself finished before neighbor prefetch. Ensure prefetch remains cancellable and counted after the visible query completes.

### Acceptance

- A pan within loaded coverage performs zero source reads and no numeric re-decoding.
- Rapid gestures keep the previous valid plot visible and converge to the latest range without stale replacement.
- Source edits and decoder changes immediately reject invalid retained data.
- Test boundary pans, resizing/DPI changes, reverse-direction pans, cancellation, and zooms that require finer data.
- Report request count, cancelled bytes/work, and p95 interaction latency for the navigation trace.

## Phase 3 — Unify numeric execution and resource ownership

### Tasks

- Route every eligible native `NumericPlan` through the same tiled cache/query service. Determine eligibility from offsets, strides, types, and segmentation rather than UI group naming.
- Preserve grouped shared reads for compatible fields and avoid duplicate reads of the same physical records.
- Expose diagnostics for the selected path: numeric tiled execution, sampled numeric execution, or full parser fallback, including why projection was rejected.
- Replace per-reader cache allowances with a shared document/application budget. Account for raw tiles, summary pages, in-flight buffers, and retained display results; separately observe OS file-cache memory.
- Give viewport work priority over indexing and prefetch. Bound I/O concurrency separately from CPU concurrency.
- Use an explicit worker pool with headroom for UI/rendering. Coordinate any CPU raster work with the overall resource policy.
- Define and document the revised meaning of `BINWAVE_PLOT_CACHE_MB` and interaction with `RAYON_NUM_THREADS`, preserving compatibility where practical.

### Acceptance

- Ordinary fixed and grouped layouts both reuse cached data when revisiting overlapping regions.
- One versus many reader groups stays within a documented global budget plus bounded, measured in-flight overhead.
- Test field selection changes without unnecessarily discarding cached unaffected fields.
- Benchmark several CPU/I/O concurrency settings; do not assume the maximum thread count is fastest.
- Unsupported layouts continue to use the parser correctly, with visible diagnostics.

## Phase 4 — Build and persist a multilevel summary index

This phase persists reduced summaries, not every decoded value. A summary can answer an overview accurately while lacking the individual samples needed for a close zoom. RAM tiles alone do not prevent re-decoding after eviction or reopening. Phase 4A implements decoded-value persistence; Phase 4B evaluates variable-record lookup costs separately.

### Tasks

- Introduce aligned, mergeable summary levels over sample ranges. Retain first/last/min/max sample identity and the validity/coverage information needed by current rendering semantics.
- Use `src/plot_index.rs` as a reference for hierarchy queries and updates. Adapt it for decoded fields, first/last samples, segmentation, incremental coverage, and paged storage rather than copying its current leaf cap unchanged.
- Choose the query level from physical pixel width and visible channel count. Traverse summaries to answer wide views; descend or read raw data for unresolved boundaries and close zooms.
- Separate durable indexing work from transient viewport requests. Prioritize the current region, retain completed coverage, and permit background continuation with cancellation and resource limits.
- Keep coarse levels resident; store fine levels in bounded pages. Choose fanout, leaf width, and storage encoding from measured memory/query/I/O tradeoffs.
- Persist completed index pages incrementally in a versioned sidecar, with atomic checkpoint/publication and resumable coverage metadata.
- Use a stable identity covering source identity, decoder content/compiled plan semantics, field identity, format version, and numeric conversion policy. Do not persist pointer addresses or assume an in-memory revision survives reopening.
- Reuse the source-validation contract. Document its guarantees and limitations; avoid requiring a fresh whole-file content hash on every open.
- Treat missing, stale, truncated, corrupt, or unwritable sidecars as disposable caches. Continue with bounded in-memory/on-demand operation.
- For fixed-layout edits, invalidate affected leaves and ancestors when byte-to-sample dependencies can be proven. Fall back to broader invalidation for structural/layout changes or uncertain dependencies.

### Acceptance

- Fully indexed, aligned broad queries read zero raw source bytes and do not walk every fine tile in the viewport.
- Random viewport queries agree with a reference full scan for extrema, ordering, and provenance. Test isolated spikes at every boundary type, constant signals, ties, invalid values, segmented layouts, and partial index coverage.
- Test interrupted builds, reopen/resume, decoder changes, byte edits, source replacement, corrupt sidecars, and unavailable cache storage.
- Measure index size/build time, warm and reopened navigation, memory residency, and raw boundary-read volume on increasingly large files.
- Establish a query work bound tied to displayed buckets, channels, hierarchy traversal, and boundary refinement rather than total visible record count.

## Phase 4A — Implement persistent decoded chunks with optional per-file retention

### Decision and dependencies

Complete the Phase 0 measurements and use the shared budget/scheduler and stable cache identity from Phases 3–4. Implement this tier separately from summary storage. A limited persistent cache is enabled by default, with measured admission/eviction policies. The per-file keep option overrides cost-based admission for newly decoded supported plot values and protects retained chunks from automatic eviction. Cheap fixed-width decoding may outperform a disk read; measure and report that tradeoff without removing the agreed retention option.

### Retention contract

- Default mode retains useful chunks within a configurable shared disk quota. Eviction is allowed, so this mode does not guarantee decode-once behavior across restarts.
- Keep mode durably records newly decoded plot values and protects existing valid chunks for that file/decoder identity. Cover at least ordinary fixed and segmented/grouped numeric layouts; document support and limitations for other decoded layouts explicitly.
- Enabling Keep does not eagerly decode the whole file or reconstruct values already lost from RAM/disk. Protect existing disk chunks and persist available compatible RAM tiles; show retained field/range coverage and extend it as the user explores.
- For valid, successfully persisted field/range coverage, repeated plot queries after RAM eviction or restart must reuse the decoded values without numeric re-decoding or source payload reads. Source identity validation and exact inspector operations may still access the source. This guarantee assumes an unchanged source/decoder and intact retained storage.
- Keep-mode storage is accounted separately from the default evictable quota and grows with retained coverage. Display its disk usage and pending persistence status. Bound RAM and write queues even though kept disk data is not automatically evicted.
- If writes fail or disk space is insufficient, keep existing retained data, visibly report that new coverage is not retained, and continue bounded plotting through the fallback path. Never silently evict kept chunks or report pending/failed writes as durable. Do not claim retention for work lost before a successful commit.
- Ordinary clear-cache removes disposable entries but preserves kept data. Provide an explicit per-file removal action. Turning Keep off makes the data eligible for normal eviction; it does not need to delete it immediately. Source/decoder invalidation still prevents stale reuse in either mode.

| Stored representation | Intended query | Limitation |
| --- | --- | --- |
| Multilevel summaries | Wide views and extrema queries | Cannot reconstruct all individual samples |
| RAM decoded tiles | Immediate close-range navigation | Eviction and restart remove reuse |
| Persistent decoded chunks | Close-range revisits after eviction/restart | Adds disk space, write traffic, and load/deserialization cost |
| Record offsets/checkpoints | Locate variable-size records without replaying the whole prefix | Does not by itself cache decoded values |

### Tasks

- Define a versioned chunk format keyed by source/decoder identity, collection, sample range, and field coverage, with eviction protection for kept chunks. Share chunks between compatible requests instead of keying them by exact viewport bounds or the entire selected field set.
- Store decoded values, validity information, and enough provenance to reproduce the plotting result. Avoid additional precision loss relative to the existing decoder. Keep exact inspector/raw-byte operations backed by the source where required.
- Route close-range queries through RAM, then eligible disk chunks, then source decoding. Broad queries should continue to use summaries. Promote useful disk hits to RAM under the global budget.
- Tune default-mode admission based on decode cost and expected reuse. In Keep mode persist newly decoded supported values regardless of that cost heuristic, without initiating unrequested whole-file decoding.
- Set a configurable evictable disk quota across documents and account for metadata, temporary writes, compressed/uncompressed sizes, and kept storage separately. Specify eviction and reclamation independently of summary retention; integrate with clear-cache and per-file removal according to the retention contract.
- Persist the per-file keep setting and protected chunk manifest across restarts. Implement coverage/usage reporting and the exact UI option “Keep decoded data for this file”. Distinguish summary coverage from retained individual values.
- Publish complete chunks atomically and validate their integrity before use. Reuse source/decoder invalidation rules; isolate edited revisions so patched values cannot contaminate an unchanged source's cache after reopen.
- Bound and schedule writes below foreground interaction work, with backpressure and visible pending retention. Read-only cache directories, quota exhaustion, interrupted writes, and unavailable space must fall back according to the retention contract.
- Compare uncompressed and compressed chunks only if their read savings justify decompression and write costs.

### Benchmarks and required acceptance

- Compare source decoding versus disk-chunk retrieval on identical navigation traces, using cheap fixed layouts and expensive supported decoder layouts. Test both warm OS-cache and storage-bound cases where available.
- Measure p50/p95 close-range latency, decoding avoided, total I/O, cache population cost, disk footprint, and break-even revisit count. Include write/read contention on the same storage device.
- A valid, complete disk hit after RAM eviction or restart performs zero numeric re-decoding and zero source payload reads for the covered plot query. Source identity validation may still occur. Record partial hits separately.
- Test overlapping views, field subsets, eviction, restart, source edits/replacement, decoder changes, corrupted chunks, and concurrent readers/writers.
- Test Keep on/off transitions, promotion of existing chunks/RAM tiles, restart of the retention setting, ordinary clear-cache, explicit per-file removal, write failures, and interrupted persistence. Default-mode eviction must never remove protected chunks from another file.
- For ordinary fixed and segmented/grouped layouts, persist selected ranges in Keep mode, force RAM eviction and default-cache pressure, restart, and verify zero numeric re-decoding for valid retained coverage. Verify new fields/unvisited ranges remain explicit misses rather than being falsely marked retained.
- Record the measured default quota, admission policy, storage format, and regression tolerance derived from baseline variability. Implement both default caching and Keep mode; this phase cannot conclude by deferring the disk cache. Report slower cases and remaining tuning work without claiming an unmeasured speedup.

## Phase 4B — Evaluate an offset/checkpoint index for variable-size layouts

### Decision and dependencies

Use the Phase 0 variable-record workloads to determine how much navigation time is spent finding record boundaries or replaying decoder state. Reuse existing `LazyArray` offset metadata where available and verify its memory growth. Offset indexing is unnecessary for fixed-stride layouts that already support direct addressing.

### Tasks

- Represent verified record boundaries with integer file offsets and sample/collection identity. Keep fine entries paged on disk and resident metadata bounded; compare dense offsets with sparse checkpoints plus bounded forward parsing.
- Preserve nesting, malformed-record, truncation, and resynchronization semantics. Never infer unverified offsets from average record sizes.
- Determine whether an offset is sufficient to restart decoding. For layouts with preceding-state dependencies, store validated resumable decoder state at checkpoints or explicitly fall back to prefix parsing. An offset alone must not bypass required validation or context.
- Build coverage incrementally, checkpoint it atomically, and reuse the stable source/decoder identity. Invalidate affected boundaries and downstream state conservatively after structural edits.
- Integrate boundary lookup with decoded-chunk provenance and summary queries without forcing a full offset scan before the first usable plot.

### Benchmark gate and acceptance

- Compare indexed lookup and subsequent decoding against the existing path for random deep seeks, repeated nearby pans, RAM eviction, and reopen.
- Measure offset/state index size, resident memory, build cost, records replayed per seek, and p95 query latency. Document the supported layouts and the actual replay bound; for unsupported stateful layouts, report the fallback cost explicitly.
- Verify sample order, byte ranges, values, and validation behavior against a sequential reference decoder across variable lengths, nested records, malformed data, and checkpoint boundaries.
- Test incomplete indexes, resume, corruption, source/decoder invalidation, and unavailable index storage.
- Record a benchmark-backed decision to implement/enable the index for supported layouts or defer it. Keep decoded-value caching and boundary indexing as separate decisions.

## Phase 5 — Optimize cache-miss scanning

### Tasks

- Add a summary-only tile scan that decodes directly into extrema accumulators instead of always materializing `Vec<Vec<f64>>` and scanning it again. When Keep mode is enabled, stream decoded values into bounded persistence chunks alongside reduction; do not discard them under a summary-only optimization or materialize the whole scan in RAM.
- Retain raw decoded tiles primarily around active close-zoom regions. Decide whether to retain other raw data from the shared budget and observed reuse.
- Compile primitive type/endian handling into typed block kernels. Validate offsets/widths once where justified, preserving checked access and conversion semantics.
- Reuse bounded input buffers; measure read size and segmented-read coalescing. Keep the browser's bounded-read interface intact.
- Optimize hot reduction loops, including repeated bucket arithmetic, only after profiling. Add SIMD for common layouts when measurements justify it and retain a portable path.
- Evaluate alternate I/O strategies against the positional-read baseline. Any mapped-file implementation needs an explicit immutable-source/snapshot lifetime model and benchmark evidence.

### Acceptance

- Summary-only and raw-materializing paths produce equivalent results for supported types, endian settings, field selections, segmentation, and cancellation points.
- Report useful input throughput, allocated bytes, memory traffic proxies where available, CPU utilization, and source read volume.
- Broad summary scans do not populate large raw-value caches without demonstrated reuse.
- Re-tune Phase 4A default-mode admission after decoding optimizations; faster source decoding can change the disk cache's break-even point. Preserve Keep-mode retention and reuse requirements.
- Background scan optimization does not regress foreground interaction latency.

## Phase 6 — Compact display data and keep rendering responsive

### Tasks

- Replace per-point path strings with compact field/collection IDs and sample indices where possible. Resolve exact paths and byte ranges on selection/inspection using retained plan metadata.
- Share immutable numeric arrays across query output and presentation. Avoid cloning sample/envelope structures just to apply scale/offset.
- Enforce a global display geometry budget across all visible collections. Count actual emitted points and envelope endpoints; a bucket budget can emit several points per bucket.
- Select mesh/raster/GPU rendering from measured geometry and pixel cost rather than channel count alone.
- Move expensive raster preparation off the UI thread, with latest-generation publication and bounded scheduling.
- Reuse GPU buffer capacity and update viewport/presentation uniforms without re-uploading unchanged data when practical.
- Preserve high-index coordinate precision when changing geometry reuse or origin rebasing, and preserve existing selection/hover behavior.

### Acceptance

- Test selection/provenance, hidden series, scale/offset changes, gaps, extreme values, high sample indices, DPI changes, and renderer fallback.
- Benchmark 1, 16, 32, 64, and many channels using both dense and sparse traces.
- Report overlay allocation volume, preparation time, GPU uploads, and p95 frame time during sustained pan/zoom.
- Rendering work must not starve viewport reads or background jobs through a shared worker pool.

## Phase 7 — Final tuning, regression checks, and documentation

- Run the deterministic navigation suite against the original baseline and each relevant execution path, including real allocated large files and user-supplied 100 GB data when available.
- Run focused native correctness tests and existing browser/shared-engine checks. Run required repository checks once implementation stabilizes; broaden testing when failures or unresolved risks justify it.
- Verify performance separately for warm application caches, RAM-evicted decoded data, warm OS caches, cold reads where available, and reopened persistent indexes/chunks. Report first accurate scan, warm navigation, and post-restart close zoom as separate workloads.
- Tune cache allocation, tile width, summary fanout, overscan, and concurrency from results.
- The release profile already uses optimization level 3, LTO, and one codegen unit. Evaluate machine-specific `target-cpu=native` and representative profile-guided optimization only after the architectural changes; record portability implications and measured gains.
- Update `docs/architecture.md`, `docs/decoder-resource-budgets.md`, and user-facing performance/cache documentation. Document fallback behavior and revised configuration semantics.

## Completion checklist

- [ ] Baseline and reproducible native navigation benchmark recorded.
- [ ] Raw and summary cache operations avoid linear scans.
- [ ] Valid prior geometry stays visible through navigation; aligned coverage is reused.
- [ ] Eligible ordinary and grouped numeric layouts share the query/cache engine.
- [ ] Memory and CPU/I/O work have coordinated, documented budgets.
- [ ] Multilevel summaries support bounded broad-view queries and resumable persistence.
- [ ] Limited persistent decoded caching is enabled by default, with measured quota/admission behavior.
- [ ] “Keep decoded data for this file” persists its setting and valid coverage across restart, protects chunks from automatic eviction, and passes reuse, cleanup, and failure tests.
- [ ] Variable-record offset/checkpoint indexing has a separate measured decision and documented state/replay limitations.
- [ ] Summary-only decoding and compact display data reduce avoidable allocations.
- [ ] Rendering avoids synchronous expensive preparation during gestures.
- [ ] Correctness, cancellation, invalidation, provenance, and shared-browser behavior verified.
- [ ] Before/after results and remaining limitations documented without unsupported speedup claims.

## Implementation status (September 26, 2026)

The implementation is in progress. The initial completed changes are in commit
`9028168`. A later stage adds constant-time tile/summary cache metadata,
in-flight tile sharing, reuse of compatible displayed data during navigation,
and native tiled execution for eligible ordinary numeric collections. It also
adds a versioned, integrity-checked decoded tile cache for local unedited files,
with a 4 GiB default evictable disk quota (`BINWAVE_PLOT_DISK_CACHE_MB`), a
per-file Keep control, pending-write status, and explicit removal. Keep covers
newly decoded selected numeric fields; it does not initiate a full scan.

These changes do **not** yet satisfy the full plan. Loaded coverage is not
independent of viewport resolution. Active readers now divide a shared RAM
tile/summary allowance, but in-flight buffers, display output, and queued disk
writes lack a coordinated global budget. The summary index is not multilevel
or persistent. Decoded chunks now share
columns across compatible field selections, but partial field hits still
decode the entire requested group before merging new columns. Variable-record
checkpoints and render-pipeline
work remain. Medium-density raster preparation now runs in one cancellable
background job, but frame-time and GPU-upload targets remain unmeasured. The
Keep control reports pending writes and disk usage, but not
field/range coverage. No 100 GB performance result or p95 interaction/frame
measurement has been collected. Do not treat a successful close-range tile
test as validation of the 100 GB acceptance gates.

A subsequent crowded-plot regression showed 0.4–9.46 second UI draw phases in
the application log as traces arrived. The GPU renderer abandoned its entire
upload when it reached the segment cap and fell back to per-trace UI meshes.
Commit `576d63b` keeps a bounded, extrema-retaining GPU representation and
prepares it off the UI thread; plot buffers are shared with raster workers.
Synthetic over-cap and invalid-gap tests pass, but live frame-time and GPU
upload measurements remain required before the rendering acceptance gate can
be marked complete.

The next Phase 2 increment separates the visible viewport from aligned numeric
query coverage. A pan within that coverage reuses its completed result; the
file-backed regression test observes zero additional source reads. Compatible
data remains paintable while a boundary pan loads. Intermediate raster results
also remain visible while a newer generation is prepared. Broad scans still
walk fine tiles because multilevel summaries are not yet implemented.

An allocated 80 MB, 20-million-sample generic `i32` file on the same Ryzen 9
7950X and tmpfs-backed `/tmp` measured 81.8 ms for its first accurate overview.
Forty nearby requests had query-latency p50 below 0.01 ms and p95 of 19.0 ms;
11 requests changed query coverage, and all 4,883 source reads (80 MB) occurred
during the overview. This is a warm-OS-cache loader measurement, not a UI frame
or gesture-to-paint measurement. Run
`BINWAVE_BENCH_SAMPLES=20000000 cargo test --release --bin binwave benchmark_plot_reader_navigation_trace -- --ignored --nocapture`
to repeat it. Retained display output, in-flight memory, GPU uploads, and
first-scan performance on a 100 GB file remain unverified.

The next safety and scheduling increment validates source identity before a
retained plot is painted, including pans that require no new query, and stops
active reads if the source has changed. Concurrent collection readers now
default to eight or the available logical cores minus two, whichever is
smaller; `BINWAVE_PLOT_WORKERS` can override that count within the headroom
limit. On the same Ryzen 9 7950X and tmpfs-backed `/tmp`, a generic allocated
8 MB, two-million-sample `i32` file measured simultaneous independent overview
reads at 16.3 ms for one reader, 24.4 ms for four, 32.1 ms for eight, and
53.7 ms for sixteen (total batch latency). This probes warm-cache throughput,
not UI responsiveness or storage-bound 100 GB behavior. Eight readers provide
parallel throughput without running one independent scan per logical core;
the default still requires frame-time and interaction benchmarking. Repeat with
`BINWAVE_BENCH_SAMPLES=2000000 cargo test --release --bin binwave benchmark_parallel_plot_readers -- --ignored --nocapture`.
The UI also now scans each resident decoded page for nested collection names
once, instead of repeating that traversal on every frame; discovered-path
deduplication uses a hash set. A focused page-reuse test passes. The effect on
live frame time with hundreds of nested collections is not yet measured.

### Limited synthetic measurement

Command: `BINWAVE_BENCH_SAMPLES=20000000 cargo test --release --bin binwave
benchmark_numeric_plot_navigation -- --ignored --nocapture`. The ignored test
allocates a temporary 80 MB file of generic little-endian `i32` values and
uses the native numeric query engine. On a Ryzen 9 7950X (16 cores/32 threads,
61 GiB RAM), Linux 7.1.13, tmpfs-backed `/tmp`, the overview took 74–78 ms,
read 80 MB from the source in 4,883 calls, and filled 4,883 RAM tiles. The
following 8,192-sample zoom, adjacent pan, and return took roughly 1.4–1.9 ms
each and made zero source read calls. These are two warm-OS-cache runs, not
p50/p95 figures. They cover one field and no UI frame rendering, disk-cache
reopen, broad field group, or storage-bound load. The benchmark needs the
additional layouts/navigation trace specified in Phase 0 before performance
gates can be set.

A summary-only broad-scan prototype was measured on the same 20 million sample
workload. It took 348 ms and read 143 MB because many display-bucket boundaries
caused a second raw-tile read, versus 74–78 ms and 80 MB on the retained
materializing path. The prototype was removed from the active path. Phase 5
needs a single-pass treatment of partial buckets before summary-only scanning
can be enabled without this regression.

## Deliverable from the implementing agent

Provide the completed code and focused tests, benchmark commands/fixtures, a before/after results table, updated resource/architecture documentation, the implemented default-cache and per-file retention behavior, an explicit decision for variable-record checkpoints, and a concise explanation of remaining bottlenecks. Distinguish warm reuse, post-eviction/restart reuse in both retention modes, and first accurate scan results. If the full 100 GB dataset or required hardware is unavailable, report the largest measured workload and leave those performance gates explicitly unverified.
