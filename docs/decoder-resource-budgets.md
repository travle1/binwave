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
parent records. Large plot overviews use a bounded sample preview, and the
Stop control cancels a running plot read until the view is changed or Fit is
requested again.

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
