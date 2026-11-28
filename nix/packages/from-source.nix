# Configurable source build helper for doggirl / Ladybird.
#
# Ladybird pins a large set of third-party libraries via vcpkg.json plus custom
# overlay ports (curl, nghttp2, ffmpeg, skia, angle, pdfjs, llvm-libc, wuffs,
# zmij) and downloads Unicode/HSTS/PSL data during CMake configuration unless
# ENABLE_NETWORK_DOWNLOADS=OFF and LADYBIRD_CACHE_DIR is populated.
#
# This derivation therefore takes pre-staged offline inputs when building inside
# the Nix sandbox:
#   - vcpkgInstalledDir: a prepared vcpkg_installed tree (from `./Meta/ladybird.py deps`)
#   - cacheDir: a pre-populated LADYBIRD_CACHE_DIR (Unicode, HSTS, Public Suffix List, etc.)
#   - or useSystemDeps = true on Linux when providing compatible system libraries.
{
  lib,
  stdenv,
  cmake,
  ninja,
  pkg-config,
  python3,
  git,
  nasm,
  perl,
  rustPlatform,
  rustToolchain,
  src,
  vcpkgInstalledDir ? null,
  vcpkgTriplet ? (
    if stdenv.hostPlatform.isDarwin then
      if stdenv.hostPlatform.isAarch64 then "arm64-osx" else "x64-osx"
    else if stdenv.hostPlatform.isAarch64 then
      "arm64-linux"
    else
      "x64-linux"
  ),
  cacheDir ? null,
  buildType ? "Release",
  enableSharedLibs ? false,
  enableUpdateCheck ? false,
  enableSparkle ? false,
  enableTesting ? false,
  extraCmakeFlags ? [ ],
  extraBuildInputs ? [ ],
}:

let
  version = "0.4.0";
  cargoDeps = rustPlatform.importCargoLock {
    lockFile = "${src}/Cargo.lock";
  };
in
stdenv.mkDerivation {
  pname = "doggirl-unwrapped";
  inherit version;

  inherit src;

  nativeBuildInputs = [
    cmake
    ninja
    pkg-config
    python3
    git
    nasm
    perl
    rustToolchain
    rustPlatform.cargoSetupHook
  ];

  inherit cargoDeps;

  buildInputs = extraBuildInputs;

  cmakeFlags = [
    "-GNinja"
    "-DCMAKE_BUILD_TYPE=${buildType}"
    "-DBUILD_SHARED_LIBS=${if enableSharedLibs then "ON" else "OFF"}"
    "-DBUILD_TESTING=${if enableTesting then "ON" else "OFF"}"
    "-DENABLE_NETWORK_DOWNLOADS=OFF"
    "-DLADYBIRD_ENABLE_UPDATE_CHECK=${if enableUpdateCheck then "ON" else "OFF"}"
    "-DLADYBIRD_ENABLE_SPARKLE=${if enableSparkle then "ON" else "OFF"}"
    "-DLADYBIRD_ENABLE_CCACHE=OFF"
    "-DLADYBIRD_VERSION=${version}"
    "-DRUST_CARGO=${rustToolchain}/bin/cargo"
    "-DRUST_RUSTC=${rustToolchain}/bin/rustc"
  ]
  ++ lib.optionals (cacheDir != null) [
    "-DLADYBIRD_CACHE_DIR=${cacheDir}"
  ]
  ++ lib.optionals (vcpkgInstalledDir != null) [
    "-DVCPKG_MANIFEST_INSTALL=OFF"
    "-DVCPKG_INSTALLED_DIR=${vcpkgInstalledDir}"
    "-DVCPKG_TARGET_TRIPLET=${vcpkgTriplet}"
    "-DVCPKG_HOST_TRIPLET=${vcpkgTriplet}"
    "-DCMAKE_PREFIX_PATH=${vcpkgInstalledDir}/${vcpkgTriplet}"
  ]
  ++ extraCmakeFlags;

  preConfigure = ''
    export LADYBIRD_SOURCE_DIR="$PWD"
    export HOME="$TMPDIR"
    ${lib.optionalString (vcpkgInstalledDir != null) ''
      export PKG_CONFIG_PATH="${vcpkgInstalledDir}/${vcpkgTriplet}/lib/pkgconfig:${vcpkgInstalledDir}/${vcpkgTriplet}/share/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    ''}
  '';

  meta = {
    description = "Ladybird hard fork browser (doggirl) built from source";
    homepage = "https://github.com/dappermint/doggirl";
    license = lib.licenses.bsd2;
    platforms = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
    mainProgram = "Doggirl";
  };
}
