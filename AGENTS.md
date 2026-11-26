# doggirl repository conventions

this repo is `doggirl` (`https://github.com/dappermint/doggirl`), a hard fork of Ladybird focused on browser performance, latency, and clean engineering

## remotes and git safety

- `origin` (`LadybirdBrowser/ladybird`) is strictly read-only upstream: never push branches, tags, or open PRs to `origin`
- `doggirl` (`dappermint/doggirl`) is our fork remote: all branch pushes and draft/ready PRs target `doggirl`
- use `git -c commit.gpgsign=false` for automated local commits and `GH_PAGER=cat` for `gh` CLI commands
- this checkout may be a partial (`blob:none`) clone: never run an un-scoped `git grep` across the whole repository (it fetches missing test blobs); scope searches to `AK Libraries Meta` or use `rg` locally
- never mention AI or co-authorship in commits, code comments, or PR bodies

## core engineering invariants

- **rust safety and FFI**:
  - never call `core::slice::from_raw_parts` or `core::slice::from_raw_parts_mut` directly; always use `ak::slice_from_raw_parts` / `ak::slice_from_raw_parts_mut`
  - every `unsafe` block needs a `// SAFETY:` comment explaining why the invariants hold
  - never expose safe `pub fn` APIs that rely on `debug_assert!` to uphold UB invariants (make them `pub unsafe fn` with `# Safety` docs, or enforce with `assert!`)
  - preserve pointer provenance (`core::ptr::with_exposed_provenance(_mut)` or `.as_ptr().cast::<T>()`, not raw `usize as *mut T`)
- **cross-platform `-Dwarnings`**:
  - `Meta/CMake/rust_crate.cmake` builds all Rust crates with `-Dwarnings` on Linux, Windows, and macOS (`x86_64` and `aarch64`)
  - any field, struct, method, or import only read under `#[cfg(target_os = "...")]` or `#[cfg(target_arch = "...")]` must also be gated under the same `#[cfg]`
  - avoid 64-bit-only `usize` literals (`0x9e37_79b9_7f4a_7c15usize`); do 64-bit mixing in `u64` and cast back
- **process-wide state and thread safety**:
  - `Meta/Linters/check_rust_process_wide_state.py` checks every mutable `static` / `thread_local!` in shared Rust crates and `libweb_rust`; register any justified pool/memo in `ALLOWED` or `RENDER_STATE_ALLOWED`
  - C++ thread-unsafe scratch pools (such as `event_path_pool`) must be `static thread_local NeverDestroyed<...>`
  - never hold raw owning pointers in `thread_local` statics (use `OwnPtr` to avoid LSan exit leaks)
  - cap retained scratch buffers (e.g. `<= 64 KiB` or `1024` entries) and clear process-wide pools from `trim_scratch()` / `clear_shaping_cache()`
- **moving GC, DOM, and spec correctness**:
  - `LIBGC_MOVE_EVERYTHING=1 ./Build/distribution/bin/test-js` must pass after any GC, JIT, runtime, or FFI change
  - `Animatable` / GC sweep callbacks must be keyed per `GC::Heap*` with RAII registration cleanup on `~Heap()`, never cleared on mid-life `CollectEverything`
  - DOM attribute fast paths must preserve `document().page().listen_for_dom_mutations()`, `accessibility_interested()`, and `editing_history_if_exists()`
  - CSS inheritance and transition overlay walks must follow flat-tree inheritance (`element_to_inherit_style_from()`), not `parent_element()`
  - never skip ECMA-262 observable hooks (such as `toJSON` on functions in `JSON.stringify`)

## repeatable workflow skills

load the matching skill under `.agents/skills/` before running multi-step workflows:

- `doggirl-build-and-test`: exact macOS build commands, toolchain env vars, dual-arch `flapc` checks, Rust crate tests, `test-js`, and `test-web`
- `doggirl-lint-and-pr-hygiene`: full CI linter gauntlet, `rustfmt`, `clippy`, `lint-commits.yml` rules, standalone PR carving, and cross-PR symbol checks
- `doggirl-perf-waves`: paired Speedometer 3.1 benchmarking protocol, frozen `.app` bundles, Time Profiler workflows, and verified pitfalls to avoid
- `doggirl-upstream-sync`: fetching `origin` and rebasing `doggirl` branches cleanly onto upstream `master`
