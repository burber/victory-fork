# Re-engineering plan: 30 fps with console graphics on RTX 4070 Ti + i5-13600K

Status 2026-11-27 (burber, branch `pc-fox-inplace`).

## Where the time goes today (Chapter 1, boat, Series S, run 301)

- 25 fps average, minimum 17; loading stalls 0.3-11 fps.
- Submitting thread: ~28 ms preparing a frame. Of that, "copies" ~13 ms, but
  this is change *detection* (write watch, page bookkeeping, guards), not
  the memcpy: run 313 removed every memcpy for 3,400 card copies and the
  number stayed at 13 ms.
- Helper threads ~87% idle: the submitting thread waits on a few long jobs
  (latency), not on throughput.
- GPU: 1-8 ms a frame in Series S; +15-28 ms in Series X (run 299), added
  to the frame instead of overlapping it.

## What was tried and measured

| Change | Result |
|---|---|
| Copy-back skipped for card copies >= 2 MB | +4 fps (kept) |
| Heap-record lock fast path | heap wait 4.6 -> 0.1 ms (kept) |
| JOIN_ACROSS, NARROW_WRITES, PLACED_ORDER | ~+1 fps (kept) |
| In-place constants heap (system memory) | GPU +18 ms, slower (off) |
| Pages bound by address + title-side card copies (INPLACE_VIEWS) | works; 15-22 fps, slower (off) |
| No change tracking at all (UNTRACKED) | 2.9 GB/frame over PCIe, 5 fps (off) |

## Plan

1. **Tell the runtime what the title writes, instead of detecting it.**
   Find the title's writers of GPU data in Ghidra (constant ring fills,
   skinning inputs, draw records, descriptor tables) and patch them in
   memory at load to append the written range to a lock-free journal.
   Card copies consume the journal; the write watch is asked only for
   memory with no known writer. Expected: most of the 13 ms detection.
2. **Pipeline the submitting thread.** Split preparing (journal ->
   ranges -> GPU copies) from recording/sending so one frame's preparing
   overlaps the previous frame's GPU work; fixes Series X's serialised GPU.
3. **Ray tracing.** Port fxceless 9a9091d (builder no longer stalls) on top
   of (1): its cost is the same detection, 8 GB a frame.
4. **Series X + upscale.** With (2), switch to Series X; restore a temporal
   upscale with motion vectors for 1080p+.
5. **Lighting / purple sheen / face blocks.** Mostly missing ray-traced GI
   and reflections (3); re-check after (3).
