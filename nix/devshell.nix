{
  pkgs,
  rustToolchain,
  doggirlConfigApply,
}:

let
  inherit (pkgs) lib stdenv;

  # On macOS, GNU libtool must be exposed as glibtool/glibtoolize (matching Homebrew)
  # so it does not shadow Apple's /usr/bin/libtool Mach-O static archiver.
  darwinGlibtool = pkgs.runCommand "darwin-glibtool" { } ''
    mkdir -p "$out/bin"
    ln -s "${pkgs.libtool}/bin/libtool" "$out/bin/glibtool"
    ln -s "${pkgs.libtool}/bin/libtoolize" "$out/bin/glibtoolize"
  '';

  commonPackages = [
    rustToolchain

    # Core build and code generation tools required by CMakeLists.txt, Meta/ladybird.py and vcpkg
    pkgs.cmake
    pkgs.ninja
    pkgs.gnumake
    pkgs.ccache
    pkgs.sccache
    pkgs.pkg-config
    pkgs.python3
    pkgs.nasm
    pkgs.autoconf
    pkgs.autoconf-archive
    pkgs.automake
    (if stdenv.hostPlatform.isDarwin then darwinGlibtool else pkgs.libtool)
    pkgs.bison
    pkgs.flex
    pkgs.perl
    pkgs.git
    (lib.getBin pkgs.curl)
    pkgs.gnutar
    pkgs.zip
    pkgs.unzip

    # Linting, formatting, search and inspection
    pkgs.llvmPackages_22.clang-tools
    pkgs.ruff
    pkgs.shellcheck
    pkgs.shfmt
    pkgs.jq
    pkgs.ripgrep
    pkgs.nixfmt
    pkgs.statix
    pkgs.deadnix

    # Config applier helper
    doggirlConfigApply
  ];

  linuxPackages = lib.optionals stdenv.hostPlatform.isLinux [
    pkgs.llvmPackages_22.clang
    pkgs.llvmPackages_22.lld
    pkgs.mold
    pkgs.qt6.qtbase
    pkgs.qt6.qtpositioning
    pkgs.qt6.qttools
    pkgs.qt6.qtwayland
    pkgs.glslang
    pkgs.libdrm
    pkgs.libGL
    pkgs.libpulseaudio
    pkgs.wayland
    pkgs.libxcb-wm
    pkgs.ncurses
    pkgs.patchelf
    pkgs.fontconfig
    pkgs.liberation_ttf
  ];

  # On macOS, mkShellNoCC avoids Nix's stdenv cc-wrapper overriding Xcode's
  # Apple Clang, Swift, actool, iconutil, codesign and MacOSX.sdk.
  mkShellFn =
    if stdenv.hostPlatform.isDarwin then
      pkgs.mkShellNoCC
    else
      pkgs.mkShell.override { stdenv = pkgs.llvmPackages_22.stdenv; };
in
mkShellFn {
  name = "doggirl-dev";

  packages = commonPackages ++ linuxPackages;

  RUSTC = "${rustToolchain}/bin/rustc";
  CARGO = "${rustToolchain}/bin/cargo";

  shellHook = ''
    if git_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
      export LADYBIRD_SOURCE_DIR="$git_root"
    fi

    ${lib.optionalString stdenv.hostPlatform.isDarwin ''
      # Remove transitive Nix clang/cctools wrappers so Xcode's Apple Clang and
      # cctools are used for macOS AppKit/Swift/vcpkg builds, while keeping
      # clang-tools 22 (clang-format, clangd, clang-tidy) active.
      filtered_path=""
      IFS=':' read -r -a path_entries <<< "$PATH"
      for entry in "''${path_entries[@]}"; do
        case "$entry" in
          /nix/store/*-clang-wrapper-*/bin|/nix/store/*-clang-[0-9]*/bin|/nix/store/*-cctools-binutils-darwin*/bin)
            continue
            ;;
        esac
        if [ -z "$filtered_path" ]; then
          filtered_path="$entry"
        else
          filtered_path="$filtered_path:$entry"
        fi
      done
      export PATH="${pkgs.llvmPackages_22.clang-tools}/bin:$filtered_path"

      unset CC CXX LD AR RANLIB STRIP NM NIX_CC NIX_BINTOOLS NIX_CFLAGS_COMPILE NIX_LDFLAGS DEVELOPER_DIR
      if [ -x /usr/bin/xcrun ]; then
        if xcode_sdk="$(/usr/bin/xcrun --sdk macosx --show-sdk-path 2>/dev/null)"; then
          export SDKROOT="$xcode_sdk"
        fi
      fi
    ''}

    ${lib.optionalString stdenv.hostPlatform.isLinux ''
      export CC="${pkgs.llvmPackages_22.clang}/bin/clang"
      export CXX="${pkgs.llvmPackages_22.clang}/bin/clang++"
    ''}

    printf 'doggirl devshell: rustc %s | cmake %s | ninja %s | clang-format %s\n' \
      "$(rustc --version | cut -d' ' -f2)" \
      "$(cmake --version | head -n1 | cut -d' ' -f3)" \
      "$(ninja --version)" \
      "$(clang-format --version | awk '{print $3}')"
  '';
}
