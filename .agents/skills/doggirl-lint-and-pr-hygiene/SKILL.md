---
name: doggirl-lint-and-pr-hygiene
description: >-
  Pre-PR linter gauntlet, formatting, clippy, cross-platform warning checks,
  commit message rules, and standalone PR carving for dappermint/doggirl.
---

# doggirl lint and PR hygiene runbook

run every check in this skill before pushing branches or marking draft PRs ready on `dappermint/doggirl`

## 1. Rust formatting and clippy

only format files modified in your diff so untouched upstream files stay clean, then verify workspace-wide `cargo fmt --check` and `clippy`:

```bash
cd /Volumes/dev/ladybird
export TOOLCHAIN_BIN="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/rustup/toolchains/1.98.0-aarch64-apple-darwin/bin"
export PATH="$TOOLCHAIN_BIN:/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/bin:/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export RUSTUP_HOME="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/rustup"
export CARGO_HOME="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/cargo"
export RUSTC="$TOOLCHAIN_BIN/rustc"
export CARGO_TARGET_DIR="/Volumes/dev/ladybird/Build/release/cargo/build"

git diff --name-only HEAD -- '*.rs' | xargs rustfmt
cargo fmt --check
cargo clippy --release --target aarch64-apple-darwin -- -D clippy::all
```

common clippy traps in Ladybird's workspace config:
- `core::slice::from_raw_parts(_mut)` is disallowed workspace-wide -> use `ak::slice_from_raw_parts(_mut)`
- `clippy::explicit_iter_loop = "deny"` -> iterate `&slice` / `&mut slice` instead of `slice.iter()` / `slice.iter_mut()`
- `clippy::too_many_arguments` -> group related parameters into a small struct
- `clippy::box_collection` -> use `Box<[T]>` instead of `Box<Vec<T>>`
- `clippy::semicolon_if_nothing_returned` -> keep trailing `;` on void statements

## 2. Ladybird CI linters

always pass changed filenames via `xargs` in zsh (unquoted `$CHANGED` triggers `File name too long`):

```bash
cd /Volumes/dev/ladybird
python3 Meta/Linters/check_rust_process_wide_state.py
git diff --name-only HEAD | xargs python3 Meta/Linters/check_newlines_at_eof.py
git diff --name-only HEAD | xargs python3 Meta/Linters/check_style.py
python3 Meta/Linters/check_rust_address_keys.py
python3 Meta/Linters/check_rust_style_value_arc.py
python3 Meta/Linters/check_libjs_rust_api.py
python3 Meta/Linters/check_libjs_includes.py
python3 Meta/Linters/check_libweb_realm_mentions.py
python3 Meta/Linters/check_fetch_engine_includes.py
bash Meta/Linters/check_debug_flags.sh
```

note on C++ formatting:
- `Meta/Linters/lint_clang_format.py` expects `clang-format-22`
- do not run `clang-format-20` or `clang-format-21` in-place; match surrounding Ladybird C++ formatting manually (4-space indent, East `const`, `// NB:` / `// OPTIMIZATION:` comments, sorted `#include` blocks)

## 3. cross-platform `-Dwarnings` audit

before pushing, inspect every `#[cfg(...)]` and `#ifdef` in your diff:
- Linux/Windows builds compile without `target_os = "macos"` -> every field, method, and helper used only on macOS must be gated with `#[cfg(target_os = "macos")]`
- x86_64 builds compile without `target_arch = "aarch64"` -> every NEON helper, LUT struct, and enum variant used only on aarch64 must be gated with `#[cfg(target_arch = "aarch64")]`
- 32-bit targets have 32-bit `usize` -> never write 64-bit hex constants with `usize` suffix
- C++ code must use portable `memcmp` (not `__builtin_memcmp`) and explicitly `#include <time.h>` when calling `clock_gettime_nsec_np` under `AK_OS_MACOS`

## 4. commit and PR rules (`lint-commits.yml` + standalone carving)

- **title format**: `Subsystem: Capitalized short summary` (`<= 72` characters, no trailing full stop, no em/en dashes)
- **body format**: wrap every line at `<= 72` characters, plain British English, lowercase prose, no em/en dashes, no generic boilerplate
- **standalone PR invariant**: every PR branched off `master` must compile cleanly on its own with zero references to symbols introduced in a different PR:
  - keep helper methods and their callers in the same PR (e.g. `Class::derive_runtime_with_finalize` + `gc/heap.rs` init-tail helpers with `LibJS` runtime; `property_metadata_at_offset` + `metadata_at_offset` with JIT snapshot; SVG dirty-attribute hooks in `Document`/`Element`/`UpdateStyle` with SVG; `Element::finalize` removal with `Animatable` sweep callback; `Page::keyboard_scroll_*` with DOM callers)
  - split `Meta/Linters/check_rust_process_wide_state.py` entries into the exact PR that introduces each static
  - verify that the union of all carved PR trees matches the verified working tree SHA
- **periodic PR merge cadence**: every 4 to 5 promoted waves, sync `doggirl/master` with `origin/master`, refresh and verify open PRs against the full linter and test gauntlet, mark them ready, and merge them into `doggirl/master` so the uncommitted working tree diff resets cleanly
- **remote safety**: push only to `doggirl` (`git push --force-with-lease doggirl ...`), never `origin`
