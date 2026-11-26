# Nix flake harness for doggirl

This repository ships a self-contained Nix flake (`flake.nix` + `nix/`) providing:

- a development shell (`devShells.<system>.default`) with the pinned Rust toolchain from `rust-toolchain.toml`, CMake, Ninja, GNU Make 4.4+, clang-tools 22, Python, vcpkg prerequisites and linters
- a configurable app wrapper and CLI launcher (`packages.<system>.doggirl` / `overlays.default`) plus an offline source-build helper (`lib.mkDoggirlFromSource`)
- declarative `nix-darwin` (`darwinModules.default`) and Home Manager (`homeManagerModules.default`) modules covering every key in `Settings.json`, `Permissions.json`, `Bookmarks.json`, local content blocker lists, CLI launch flags, runtime env vars and macOS `NSUserDefaults`
- automated flake checks (`checks.<system>.*`) validating module evaluation, C++ schema parity, config activation modes, launcher wrapping, formatting and linting

## Development shell

Enter the shell from the repo root:

```bash
nix develop
```

On macOS (`aarch64-darwin`; Intel `x86_64-darwin` requires overriding the `nixpkgs` input to `nixpkgs-26.05-darwin` as `nixpkgs-unstable` dropped `x86_64-darwin`), the shell uses `mkShellNoCC` so Xcode's Apple Clang, Swift, `actool`, `iconutil`, `codesign` and `MacOSX.sdk` stay untouched while Nix provides the pinned Rust `1.98.0` toolchain (`rustfmt`, `clippy`), CMake, Ninja, GNU Make 4.4+, `ccache`, `sccache`, `pkg-config`, `nasm`, autotools, `bison`, `flex`, `perl`, `python3`, `llvmPackages_22.clang-tools` (`clang-format` 22, `clangd`, `clang-tidy`), `ruff`, `shellcheck`, `shfmt`, `jq`, `ripgrep`, `nixfmt`, `statix` and `deadnix`.

On Linux (`aarch64-linux` / `x86_64-linux`), the shell also provides Clang 22, `lld`, `mold`, Qt 6, Vulkan/GL/Wayland/XCB headers, PulseAudio and Liberation fonts.

Inside the shell, build and test as usual:

```bash
./Meta/ladybird.py build --preset Distribution
./Meta/ladybird.py test --preset Distribution
```

## Packaging and wrapping

### Local or prebuilt bundle wrapper (`packages.<system>.doggirl`)

`packages.<system>.default` (alias `doggirl`) builds a lightweight launcher providing:

- `$out/bin/doggirl` and `$out/bin/ladybird`
- `$out/Applications/Doggirl.app` on macOS (with `Info.plist` URL handlers and `app_icon.icns` generated from `UI/Icons/macos/app_icon.iconset`)
- `$out/share/applications/doggirl.desktop` on Linux

Because Ladybird pins third-party dependencies through `vcpkg.json` plus custom overlay ports (`curl`, `nghttp2`, `ffmpeg`, `skia`, `angle`, `pdfjs`, `llvm-libc`, `wuffs`, `zmij`) and requires Xcode tooling on macOS, wrapping either a locally compiled `Ladybird.app` (`localAppPath`) or a prebuilt store derivation (`unwrappedPackage`) lets `darwin-rebuild switch` stay fast while keeping all launch flags (`--profile`, `--user-agent-preset`, `--disable-features`, etc.) and environment variables (`LIBJS_JIT`, `LIBJS_CODE_FLUSH`, `LIBGC_*`) declarative.

When neither `localAppPath` nor `unwrappedPackage` is pinned, the launcher checks `DOGGIRL_APP_PATH`, `LADYBIRD_APP_PATH`, `./Build/{distribution,release,debug}/bin/Ladybird.app` and `/Applications/Ladybird.app`.

### Offline source-build helper (`lib.mkDoggirlFromSource`)

For reproducible sandboxed builds when you have staged `vcpkg_installed` and `LADYBIRD_CACHE_DIR` inputs (or compatible Linux system libraries), call `lib.mkDoggirlFromSource`:

```nix
doggirl.lib.mkDoggirlFromSource pkgs {
  rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
  vcpkgInstalledDir = /path/to/vcpkg_installed;
  cacheDir = /path/to/Build/caches;
}
```

This passes `-DENABLE_NETWORK_DOWNLOADS=OFF`, `-DVCPKG_MANIFEST_INSTALL=OFF` and vendors Rust crates offline from `Cargo.lock` via `rustPlatform.importCargoLock`.

## Exact macOS and Linux profile paths

Verified against `Libraries/LibCore/StandardPaths.cpp`, `Libraries/LibWebView/Profile.cpp`, `Libraries/LibWebView/Application.cpp` and `UI/Qt/Application.cpp`:

- **Config root**: `~/Library/Preferences` on macOS (unless `XDG_CONFIG_HOME` is set), `~/.config` on Linux
- **Data root**: `~/Library/Application Support` on macOS (unless `XDG_DATA_HOME` is set), `~/.local/share` on Linux
- **Named profile files** (`<name>` matching `[a-z0-9_-]+`):
  - `<configRoot>/Ladybird/Profiles/<name>/Settings.json`
  - `<configRoot>/Ladybird/Profiles/<name>/Permissions.json`
  - `<configRoot>/Ladybird/Profiles/<name>/Bookmarks.json`
  - `<dataRoot>/Ladybird/Profiles/<name>/ContentBlocking/Lists/<identifier>.txt`
- **Default profile name**: local development builds (`LADYBIRD_VERSION` unset) use `developer`; release builds (`LADYBIRD_VERSION` set) use `default`
- **Custom path profile** (`--profile-path /abs/path`):
  - `<path>/config/{Settings,Permissions,Bookmarks}.json`
  - `<path>/data/ContentBlocking/Lists/<identifier>.txt`

## `nix-darwin` configuration example

Add the flake input and import `doggirl.darwinModules.default`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nix-darwin.url = "github:nix-darwin/nix-darwin";
    doggirl.url = "github:dappermint/doggirl";
  };

  outputs = { nix-darwin, doggirl, ... }: {
    darwinConfigurations.my-mac = nix-darwin.lib.darwinSystem {
      system = "aarch64-darwin";
      modules = [
        doggirl.darwinModules.default
        {
          system.primaryUser = "apollo";

          programs.doggirl = {
            enable = true;
            localAppPath = "/Volumes/dev/ladybird/Build/distribution/bin/Ladybird.app";
            appName = "Doggirl";
            defaultProfile = "developer";
            manageMode = "merge"; # or "overwrite" / "symlink"
            backupExisting = true;

            launch = {
              jit = {
                enable = true;
                threshold = 400;
                warmup = 8;
              };
              webContentMemoryLimitMiB = 4096;
            };

            macosDefaults = {
              windowDragOnGesture = true;
              disableSparkleAutoChecks = true;
            };

            profiles.developer = {
              firstRunComplete = true;
              newTabPageURL = "about:newtab";
              restorePreviousSession = true;
              languages = [ "en-GB" "en" ];

              appearance = {
                showBookmarksBar = "onHover";
                interfaceTheme = "dark";
                websiteColorScheme = "dark";
                colorTheme = "lavender";
              };

              content = {
                defaultZoomLevelFactor = 1.0;
                zoomPerHost."example.com" = 1.15;
                enableForceDark = false;
              };

              tabs = {
                verticalTabsEnabled = true;
                verticalTabsExpanded = true;
                verticalTabsPosition = "left";
                discardHiddenTabsAfterMinutes = 120;
                tabProcesses = "perOrigin";
              };

              searchEngine = {
                engine = "Kagi";
                suggestions = true;
              };

              autoplay = {
                policy = "block-audio";
                siteFilters."https://www.youtube.com" = "allow-audio-and-video";
              };

              permissions = {
                geolocation.policy = "block";
                microphone = {
                  policy = "ask";
                  sites."https://meet.example.com" = "allow";
                };
                notifications.policy = "block";
              };

              globalPrivacyControl = true;
              crashReportSending = "never";

              backgroundNetworking = {
                enabled = true;
                contentBlockerSubscriptionUpdates = true;
                updateChecks = false;
              };

              contentBlockers = {
                enabled = true;
                builtInLists = {
                  easyList = true;
                  easyPrivacy = true;
                  easyListCookie = true;
                };
              };

              dns = {
                mode = "custom";
                server = "1.1.1.1";
                type = "tls";
                dnssec = true;
              };

              configVariables = {
                showAdvancedDebugMenu = true;
                showTabPerformanceMonitor = true;
                maximumDiskCacheSizeMb = 4096;
              };

              bookmarks = [
                { title = "Ladybird"; url = "https://ladybird.org/"; }
                {
                  title = "Dev";
                  children = [
                    { title = "doggirl"; url = "https://github.com/dappermint/doggirl"; }
                  ];
                }
              ];
            };
          };
        }
      ];
    };
  };
}
```

## Home Manager usage

Import `doggirl.homeManagerModules.default` in your Home Manager configuration; all `programs.doggirl.*` options (except `user`, `group`, `homeDirectory` and `macosDefaults`) are shared with the `nix-darwin` module.

## Verification

Run the full flake check suite:

```bash
nix flake check
```
