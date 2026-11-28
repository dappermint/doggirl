# developing doggirl

doggirl's fork is [dappermint/doggirl](https://github.com/dappermint/doggirl). use that repository for changes and bug reports. [ladybird](https://github.com/LadybirdBrowser/ladybird) is the upstream source

## build and test

prepare the dependencies using the [build instructions](BuildInstructionsLadybird.md) or the [nix development shell](Nix.md). build the browser and run the suites from the repo root

```sh
cmake --build Build/distribution
export LADYBIRD_SOURCE_DIR="$PWD"
Build/distribution/bin/test-js --suite runtime
LIBGC_MOVE_EVERYTHING=1 Build/distribution/bin/test-js
Build/distribution/bin/test-web --test-path "$PWD/Tests/LibWeb"
```

use `test-web -f` with a test name or glob for a focused run. test libjs, the jit and gc changes with moving gc enabled. compare performance with both release builds on the same machine and record the score range across repeated runs

## engine changes

keep rust ffi pointer provenance and document the invariants for unsafe operations. layout and style optimisations need to preserve invalidation, including flat-tree inheritance and updates when dom nodes move. javascript fast paths must preserve observable calls and property access. run the focused regression tests and the full affected suite before merging

## upstream updates

fetch ladybird into the read-only upstream remote and merge into a fork branch. resolve conflicts there and test the merged tree before updating fork master. use [doggirl's issue tracker](https://github.com/dappermint/doggirl/issues) for regressions specific to the fork
