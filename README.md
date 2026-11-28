# doggirl

doggirl is [dappermint](https://github.com/dappermint)'s hard fork of [ladybird](https://github.com/LadybirdBrowser/ladybird), an independent web browser built around its own standards-based engine. the fork focuses on browser performance and interaction latency. it keeps ladybird's [multi-process architecture](Documentation/ProcessArchitecture.md) and works on hot paths in the javascript engine, layout, style and rendering

v0.4.0 includes faster aarch64 jit code generation, reuse of layout and paint work, and lower overhead for style and dom updates. [release history](https://github.com/dappermint/doggirl/releases)

> [!IMPORTANT]
> doggirl is pre-alpha. expect compatibility gaps and crashes

## install on apple silicon

the nix flake installs a prebuilt macOS bundle on `aarch64-darwin` using a SHA-256 pin. try the release without installing it

```sh
nix run github:dappermint/doggirl/v0.4.0#doggirl
```

install the same release in your user profile

```sh
nix profile install github:dappermint/doggirl/v0.4.0#doggirl
doggirl
```

for a declarative install, pin the same release as a flake input and import the [nix-darwin or home manager module](Documentation/Nix.md)

## v0.4.0 test results

| check | result |
| --- | --- |
| libweb | 20,591 passed, 57 failed, 2 crashed, 118 skipped |
| libjs runtime | 9,292 passed, 1 failed, 3 expected failures. the runtime results matched with moving gc forced |
| libjs ast and bytecode | all 101 ast and 171 bytecode test files passed with moving gc forced |
| nix on apple silicon | all 7 flake checks passed. the packaged browser loaded an http page and passed javascript, flex layout and canvas checks |

all 59 libweb failures and crashes reproduced on the previous fork master build (`6e8c319390`). the [raw test summary](https://github.com/dappermint/doggirl/releases/download/v0.4.0/test-results.json) lists the affected tests

## speedometer 3.1

tested on 28 november 2026 on an apple m5 pro running macOS 27.3. each browser ran headless with a fresh profile and a 1280 × 900 css-pixel viewport at scale 1. we ran eight blocks with one launch per browser in each block, counterbalanced the order, and scored ten full iterations per launch. the table shows the median and range of the eight launch scores. [benchmark](https://browserbench.org/Speedometer3.1/)

| browser | median score | launch range |
| --- | ---: | ---: |
| doggirl v0.4.0 | 33.95 | 32.03 to 34.33 |
| upstream ladybird [`690336455f`](https://github.com/LadybirdBrowser/ladybird/commit/690336455f478a3ee00403e2ef40b494550b8bc3) | 23.11 | 22.06 to 23.37 |
| chromium 160.0.8190.0, snapshot `1756980` | 47.64 | 45.78 to 48.11 |
| firefox 161.0 | 53.34 | 48.90 to 53.69 |

doggirl scored +46.2% versus upstream ladybird across matched blocks (95% interval +44.6% to +47.8%). these results apply to the builds and headless settings above. [raw runs and build hashes](https://github.com/dappermint/doggirl/releases/download/v0.4.0/speedometer-3.1.json)

## build and contribute

[build instructions](Documentation/BuildInstructionsLadybird.md) cover the source build. see the [fork's repository conventions](AGENTS.md) for its build and test workflow and [ladybird's documentation](Documentation/) for the engine. report fork-specific bugs in [doggirl issues](https://github.com/dappermint/doggirl/issues)

## licence

doggirl is distributed under the [two-clause bsd licence](LICENSE). the release includes source archives for bundled lgpl libraries
