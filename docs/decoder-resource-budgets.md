# Decoder resource budgets and lazy collection visibility

## Current behavior

The YAML decoder limits eager parsing to 20,000 nodes (`MAX_NODES`). Fixed-size
collections above 128 items are represented as lazy arrays. The native Samples
view requests 128 rows at a time; the shared reader permits up to 512 rows per
request. These are implementation limits, not limits of a file format.

A display page can contain many individually valid records. The parser must
apply its per-record safety limit to each record, while independently bounding
the total memory retained by a page. An error while loading a lazy page must
never leave an expanded collection looking empty without an explanation.
The Structure view retains a small set of lazy pages so expanding a child
collection does not evict the parent collection needed to display it.
For fixed-size nested collections, the Samples and Plot views can also expose
one virtual collection spanning all parent records. It retains the original
byte locations and decodes only requested pages. Plot discovers this view from
one parent record in the background, without requiring tree expansion.
Fixed numeric layouts use a batched plot reader that skips the gaps between
parent records. Plot overviews scan the visible range for min/max peaks by
default, with bounded output and progress while reading. An optional fast
sampled preview in Plot options can miss peaks. The Stop control cancels a
running plot read until the view is changed or Fit is requested again.
Selected fields from one compatible group, including the primary plotted
field, share one scan. Grouped numeric tiles load in parallel and are merged
in sample order.
The RAM tile/summary cache budget is chosen from available memory on Linux and
Windows (capped at 1 GiB total for active plot readers), with a 256 MiB
fallback elsewhere. Set `BINWAVE_PLOT_CACHE_MB` to choose 64–4096 MiB shared
across active readers. Each reader receives a share; 75% of its share is for
raw decoded tiles and 25% for summaries. In-flight scan batches are bounded
separately by half that share or one tile. Parallel scan batches cap their
tile count below the reported logical CPU count when possible; the Rayon pool
itself still uses the available threads. `RAYON_NUM_THREADS` can override the
pool's thread count. At most eight collection readers run concurrently by
default, leaving two reported logical cores as headroom. Set
`BINWAVE_PLOT_WORKERS` to choose 1 through that headroom limit. This bounds
reader concurrency; each active reader can still submit tile work to Rayon.
Native numeric tiles now use direct hash lookups and an LRU order, and eligible
ordinary collections use the same tiled path as grouped fields. Compatible
prior plot data stays visible while a new viewport request loads. The RAM
allowance is divided among selected collection readers. Retained completed
plot output, queued disk writes, and in-flight tiles are not included in this
RAM cache allowance; a full global scheduler is still needed.
Native numeric plots request aligned coverage around the visible viewport.
Small pans inside that coverage reuse the same result without new source reads.
The plot checks file identity before displaying retained coverage and stops
active scans if the original file has changed.
For many visible traces, the plot reduces per-series min/max resolution toward
an approximately one-million-point display target; each bucket preserves its
observed extrema. Exact views and the minimum bucket count can exceed this
target. This reduces retained display geometry but does not bound decoded tile
memory, in-flight work, or the first accurate scan. Raster and GPU geometry
workers keep the prior valid image visible while preparing an update.
Numeric plot points now share the fixed parts of each field path and retain
the sample index; the full path string is formatted only for selection or
inspection. This removes per-point path-string allocations from numeric
query output. Byte ranges and exact sample indices remain attached to points.

For unchanged local files without byte edits, close-range numeric tiles can
also be stored as versioned decoded chunks under the app cache directory.
`BINWAVE_PLOT_DISK_CACHE_MB` sets the evictable disk quota in MiB (default
4096, clamped to 512–65536). The Plot options menu has **Keep decoded data for
this file**. It protects already stored chunks and persists newly decoded
selected numeric values as the file is explored. Pending writes and retained
disk usage are displayed there; a failed write is reported and plot reads
continue from the source. Ordinary cache clearing preserves kept chunks; the
same menu provides explicit per-file removal. Compatible field subsets reuse
stored columns. A request combining retained and new fields currently decodes
the whole requested group, then merges new columns into the chunk.
The disk quota and Keep storage do not limit the source file itself.

## Follow-up work

1. Measure the memory occupied by decoded nodes, values, and cached pages.
   Raw byte count alone is not an adequate estimate of tree memory.
2. Load pages incrementally. Choose the number of rows from a configurable
   page-memory budget and observed record cost; wide records should receive
   smaller pages. Keep navigation and plotting lazy and cancellable.
3. Preserve a modest default budget. Consider an optional higher-resource
   setting after measuring actual use. Available system RAM can inform the
   default but must not be treated as a guaranteed allocation.
4. Report every budget hit and lazy-page failure in the affected view and the
   decoder status or console. Include the collection path, operation, limit,
   amount reached, and a useful next step. Do not silently drop rows or fields.
5. Extend nested-page coverage to the browser reader and deeper nesting.
   Verify page-cache memory use and eviction under sustained navigation.
6. Extend virtual collections to variable-size nested layouts with verified
   boundaries and explicit memory limits for any required offset index.

## Acceptance checks

- A wide valid record expands into visible fields and remains navigable.
- A page budget produces a visible, actionable message or a smaller page.
- A malformed or excessive record fails with its path and reached limit.
- Paging, selection, and plotting do not materialize the complete file.
- No fixture or decoder for a private file format is stored in the repository.
