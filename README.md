# doggirl

<img src="UI/Icons/macos/app_icon.iconset/icon_128x128.png" alt="doggirl paw logo" width="128" height="128">

doggirl is [dappermint](https://github.com/dappermint)'s hard fork of [ladybird](https://github.com/LadybirdBrowser/ladybird), an independent web browser built around its own standards-based engine. the fork focuses on browser performance and interaction latency. it keeps ladybird's [multi-process architecture](Documentation/ProcessArchitecture.md) and works on hot paths in the javascript engine, layout, style and rendering

v0.4.1 fixes crashes and regressions in layout, rendering and javascript. it also updates the engine to ladybird [`690336455f`](https://github.com/LadybirdBrowser/ladybird/commit/690336455f478a3ee00403e2ef40b494550b8bc3) and adds three terrier barks to the about page. [release history](https://github.com/dappermint/doggirl/releases)

> [!IMPORTANT]
> doggirl is pre-alpha. expect compatibility gaps and crashes

## install on apple silicon

the nix flake installs a prebuilt macOS bundle on `aarch64-darwin` using a SHA-256 pin. try the release without installing it

```sh
nix run github:dappermint/doggirl/v0.4.1#doggirl
```

install the same release in your user profile

```sh
nix profile install github:dappermint/doggirl/v0.4.1#doggirl
doggirl
```

### add doggirl to your flake

pin the release in your `flake.nix` inputs

```nix
inputs.doggirl.url = "github:dappermint/doggirl/v0.4.1";
```

add `doggirl` to your `outputs` arguments. add the module and option below to your existing `nix-darwin` configuration's `modules` list

```nix
modules = [
  doggirl.darwinModules.default
  { programs.doggirl.enable = true; }
  # your other modules
];
```

apply your configuration with `sudo darwin-rebuild switch --flake .#my-mac`, using your configuration name in place of `my-mac`. the module installs the prebuilt bundle and exposes settings for profiles and launch flags. [nix module options](Documentation/Nix.md)

for a package list, use `doggirl.packages.aarch64-darwin.doggirl` in `environment.systemPackages` or `home.packages`

## v0.4.1 test results

| check | result |
| --- | --- |
| libweb | 20,662 passed, 0 failed, 0 crashed, 118 skipped |
| libjs runtime | 9,293 passed and 3 expected failures. the results matched with moving gc forced |
| libjs ast and bytecode | all 101 ast and 171 bytecode test files passed with moving gc forced |
| nix on apple silicon | all 7 flake checks passed. the packaged browser passed http, javascript, flex layout, canvas and about-page audio checks |

all 59 libweb cases that failed or crashed in v0.4.0 passed in v0.4.1. [test results](https://github.com/dappermint/doggirl/releases/download/v0.4.1/test-results.json)

## speedometer 3.1

### v0.4.1 against v0.4.0 and upstream

v0.4.1 scored within run-to-run variation of v0.4.0 and led upstream ladybird in all six blocks

| browser | median score | launch range |
| --- | ---: | ---: |
| doggirl v0.4.1 | 32.45 | 31.68 to 33.07 |
| doggirl v0.4.0 | 32.18 | 31.47 to 33.41 |
| upstream ladybird [`690336455f`](https://github.com/LadybirdBrowser/ladybird/commit/690336455f478a3ee00403e2ef40b494550b8bc3) | 22.05 | 21.30 to 22.55 |

the paired score change was +0.2% (95% confidence interval -2.2% to +2.7%) against v0.4.0 and +47.2% (95% confidence interval +44.0% to +50.4%) against upstream

the published doggirl bundles and upstream ladybird ran on the same apple m5 pro with macOS 27.3 on 28 november 2026. each build ran three warm-up iterations in a separate launch. the six blocks covered all six launch orders. each scored launch ran headless with a fresh profile and a 1280 × 900 css-pixel viewport at device scale 1. all 20 workloads completed in every iteration. a launch score is the mean of its ten scored iterations. [benchmark](https://browserbench.org/Speedometer3.1/)

<details>
<summary>scores by block</summary>

| block | doggirl v0.4.1 | doggirl v0.4.0 | upstream ladybird |
| ---: | ---: | ---: | ---: |
| 1 | 31.68 | 31.50 | 22.08 |
| 2 | 32.86 | 32.35 | 22.55 |
| 3 | 32.60 | 33.41 | 22.27 |
| 4 | 31.78 | 31.47 | 21.78 |
| 5 | 32.29 | 33.14 | 21.30 |
| 6 | 33.07 | 32.02 | 22.01 |

</details>

### v0.4.0 against upstream, chromium and firefox

the v0.4.0 comparison ran on 28 november 2026 on an apple m5 pro running macOS 27.3. each browser ran headless with a fresh profile and a 1280 × 900 css-pixel viewport at scale 1. we ran eight blocks with one launch per browser in each block, counterbalanced the order, and scored ten full iterations per launch. the table shows the median and range of the eight launch scores. [benchmark](https://browserbench.org/Speedometer3.1/)

| browser | median score | launch range |
| --- | ---: | ---: |
| doggirl v0.4.0 | 33.95 | 32.03 to 34.33 |
| upstream ladybird [`690336455f`](https://github.com/LadybirdBrowser/ladybird/commit/690336455f478a3ee00403e2ef40b494550b8bc3) | 23.11 | 22.06 to 23.37 |
| chromium 160.0.8190.0, snapshot `1756980` | 47.64 | 45.78 to 48.11 |
| firefox 161.0 | 53.34 | 48.90 to 53.69 |

doggirl scored +46.2% versus upstream ladybird across matched blocks (95% interval +44.6% to +47.8%). these results apply to the builds and headless settings above. [raw runs and build hashes](https://github.com/dappermint/doggirl/releases/download/v0.4.0/speedometer-3.1.json)

## build and contribute

[build instructions](Documentation/BuildInstructionsLadybird.md) cover the source build. see the [fork's development guide](Documentation/DoggirlDevelopment.md) for its build and test workflow and [ladybird's documentation](Documentation/) for the engine. report fork-specific bugs in [doggirl issues](https://github.com/dappermint/doggirl/issues)

## licence

doggirl is distributed under the [two-clause bsd licence](LICENSE). the release includes source archives for bundled lgpl libraries
