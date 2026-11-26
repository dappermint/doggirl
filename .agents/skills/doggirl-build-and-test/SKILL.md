---
name: doggirl-build-and-test
description: >-
  Repeatable build, Rust unit test, dual-arch flapc, test-js, and test-web
  verification commands for the doggirl Ladybird fork on macOS Apple Silicon.
---

# doggirl build and test runbook

use this procedure whenever building `Build/distribution` or verifying changes across Rust, C++, JS, and LibWeb

## 1. toolchain environment (macOS perf workspace)

always export the local pinned Rust toolchain before running `cmake`, `cargo`, `rustfmt`, or `flapc`:

```bash
export TOOLCHAIN_BIN="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/rustup/toolchains/1.98.0-aarch64-apple-darwin/bin"
export PATH="$TOOLCHAIN_BIN:/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/bin:/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export RUSTUP_HOME="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/rustup"
export CARGO_HOME="/Volumes/dev/ladybird-perf.ojgesEaE/toolchain/cargo"
export RUSTC="$TOOLCHAIN_BIN/rustc"
export CARGO_PROFILE_RELEASE_STRIP=none
```

notes:
- the primary CMake build directory is `Build/distribution` (not `Build/release`)
- if running standalone `cargo clippy` or `cargo test`, pass `CARGO_TARGET_DIR="/Volumes/dev/ladybird/Build/release/cargo/build"` (or `Build/distribution/cargo/build`) so targets do not pollute repo root

## 2. build commands

full incremental distribution build:

```bash
cd /Volumes/dev/ladybird
cmake --build Build/distribution -j18
```

dual-architecture `flapc` smoke check whenever `Libraries/LibJS/Interpreter/interpreter.flap` changes:

```bash
cargo run --release --manifest-path Libraries/LibJS/Flap/Cargo.toml --bin flapc -- \
  --arch x86_64 --profile js \
  --constants Libraries/LibJS/Flap/tests/interpreter-layout.conf \
  --input Libraries/LibJS/Interpreter/interpreter.flap \
  --output /tmp/interp_x86_64.S

cargo run --release --manifest-path Libraries/LibJS/Flap/Cargo.toml --bin flapc -- \
  --arch aarch64 --profile js \
  --constants Libraries/LibJS/Flap/tests/interpreter-layout.conf \
  --input Libraries/LibJS/Interpreter/interpreter.flap \
  --output /tmp/interp_aarch64.S

rm -f /tmp/interp_x86_64.S /tmp/interp_aarch64.S
```

## 3. Rust unit tests

standalone crates (no C++ archive link dependency):

```bash
CARGO_TARGET_DIR="/Volumes/dev/ladybird/Build/release/cargo/build" \
cargo test --release --target aarch64-apple-darwin \
  -p libgc_rust -p libgfx_rust -p libjs_jit -p libweb_html_tokenizer -p libcompositing_rust --lib
```

note: `libregex_rust` calls `LibUnicode` C++ FFI (`unicode_*`) and is exercised end-to-end by `test-js --suite runtime` (which runs the full ECMAScript RegExp test suite)

## 4. LibJS test suites

always set `LADYBIRD_SOURCE_DIR=/Volumes/dev/ladybird` and remove the stray `test-js-runtime.log` if created in the repo root:

```bash
export LADYBIRD_SOURCE_DIR="/Volumes/dev/ladybird"
./Build/distribution/bin/test-js --suite runtime
LIBGC_MOVE_EVERYTHING=1 ./Build/distribution/bin/test-js
rm -f /Volumes/dev/ladybird/test-js-runtime.log
```

both commands must report `0` failures (`LIBGC_MOVE_EVERYTHING=1` catches moving-GC pointer/rooting bugs)

## 5. LibWeb `test-web` suite

rules for `test-web`:
- always `cd /tmp` before invoking `test-web`, pass `--test-path /Volumes/dev/ladybird/Tests/LibWeb`, and delete `/tmp/test-dumps` afterward so dumps never dirty `/Volumes/dev/ladybird`
- never pass `-f '*input*'` (every LibWeb test path contains `input/`, so it matches all 20,659 tests)
- pass `-v -v` (two separate flags, not `-vv`) when debugging a crash or failure

targeted run:

```bash
cd /tmp && rm -rf /tmp/test-dumps
/Volumes/dev/ladybird/Build/distribution/bin/test-web \
  --test-path /Volumes/dev/ladybird/Tests/LibWeb \
  -f '*fragment-parser-fast-path*' -v -v
rm -rf /tmp/test-dumps
```

full suite (~2 to 3 minutes on 18 cores):

```bash
cd /tmp && rm -rf /tmp/test-dumps
/Volumes/dev/ladybird/Build/distribution/bin/test-web \
  --test-path /Volumes/dev/ladybird/Tests/LibWeb
rm -rf /tmp/test-dumps
```

known upstream discrepancy on base `1df5090623` (not caused by doggirl patches):
- `Text/input/canvas/export-offscreencanvas.html` outputs 329 vs 312 PNG bytes (upstream commit `36d7e597bf`)
- every other test must pass with `0` crashes, `0` timeouts, and `0` new failures
