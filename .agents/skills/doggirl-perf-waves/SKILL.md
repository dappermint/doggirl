---
name: doggirl-perf-waves
description: >-
  Paired Speedometer 3.1 benchmarking protocol, Time Profiler workflow,
  correctness invariants, and verified pitfalls for performance waves in doggirl.
---

# doggirl performance wave runbook

use this procedure when profiling, implementing, and validating Speedometer 3.1 performance and latency waves

## 1. paired benchmarking protocol (zero hand-waving)

every performance wave must be validated against a frozen baseline `.app` bundle before keeping the change:

1. **freeze the candidate bundle**: copy `Build/distribution/bin/Ladybird.app` to `/Volumes/dev/ladybird-perf-run/<wave>-Ladybird.app` before starting any benchmark run (and verify `WebContent` SHA-256 differs from baseline)
2. **check machine noise first**: verify no stray `WebContent`, `Ladybird`, `clang`, `rustc`, or background builds are consuming CPU, and check thermal pressure (`pmset -g therm`)
3. **run alternating pairs**: use `/Volumes/dev/ladybird-perf.ojgesEaE/benchmark-pair.py` (or alternating A/B, B/A runs of `measure.py`, `>= 4` pairs for screening, `>= 6-8` pairs for final wave sign-off) with 10 Speedometer 3.1 iterations per run
4. **inspect `browser.log` and subtests**:
   - confirm `0` non-benign log lines, crashes, assertion failures, or JS exceptions in `browser.log`
   - inspect per-subtest deltas, not just overall score: catch regressions on specific workloads (such as `TodoMVC-Lit`, `TodoMVC-WebComponents`, `Charts-chartjs`, `Editor-CodeMirror`, `NewsSite-Next/Nuxt`)
5. **reject regressions promptly**: if a change is flat within noise (`< 0.5%`) or regresses a hot subtest, revert or refine it rather than accumulating complexity

## 2. profiling workflow

- profile with `xctrace` / `sample` / `stacks.pkl` on the frozen release bundle (`CARGO_PROFILE_RELEASE_STRIP=none` preserves Rust symbols)
- always target the largest verified main-thread gaps first rather than micro-optimising cold paths
- current high-value target areas against Chromium 157 (`61.69` median / `386.1 ms/iter`; target `>= 49.35`):
  - JS property-access slow paths and megamorphic inline caches (`get_by_id`, `put_by_id`, `get_by_value`, `put_by_value`)
  - `ProxyObject::internal_get`
  - layout passes (`layout_after_probe`, `TreeBuildJob::run`, `AbsposEngine::replay`)
  - style updates (`run_style_update`, `record_subtree_arrivals`)
  - paint timing (`PaintTiming::mark_paint_timing` checking `is_non_empty` before `paintable_client_rect`)

## 3. correctness and memory invariants easy to break during perf work

- **moving GC (`LIBGC_MOVE_EVERYTHING=1`)**: never stash raw `Cell*` across allocations or GC safepoints; verify `DeferGc` scopes and trace edges
- **stack frame setup in hot templates**: check early exits before constructing large inline `Vector<T, N>` buffers on the stack (e.g. `ShadowRoot.h` must check `!root_shadow && !root_first_child` before declaring `Vector<Frame, 32>`)
- **bounded scratch retention**: any reused `Vec` / scratch buffer (display lists, encoded paths, style traversal pools) must cap retained capacity (e.g. `64 KiB` or `1024` entries) and release on `trim_scratch()` / `clear_shaping_cache()`, and external memory held by GC cells must be reported in `external_memory_size()`
- **direct-mapped caches**: when batching cache misses alongside hits in one pass (such as `GLYPH_OUTLINE_CACHE`), never overwrite a colliding slot before consuming the cached hit from the same batch
- **bloom filters**: counting bloom filters (like `AncestorFilter`) saturate at `u8::MAX` and cannot be decremented back to 0; always `.clear()` unconditionally when resetting a pooled filter
- **DOM and CSS edge cases**:
  - detached parser fast paths must preserve `listen_for_dom_mutations()` and allow `rare_data()` created by earlier attributes on the same element (assert `!rare_data()->registered_observer_list && !element_rare_data()->class_list`)
  - `:has()` waiting-children arrival checks must still run when `style_engine.may_have_waiting_children()` is true
  - SVG paint resources must mark the document in both constructors and `adopted_from()`
  - JIT escape analysis (`is_call_without_frames`) and `frame_initialization` must stay in sync whenever adding or handling `Op::Call*` variants
  - JIT unprofiled inlined callees without `ExecutableFeedback` must set `ExecutableSnapshot::unprofiled = true` so `builder::has_run()` still treats instructions with feedback slots as unexecuted without allocating zero-filled feedback arrays
