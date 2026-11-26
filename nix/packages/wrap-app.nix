{
  lib,
  stdenv,
  runCommand,
  bash,
  libicns,
  repoRoot,
  unwrapped ? null,
  localAppPath ? null,
  appName ? "Doggirl",
  bundleIdentifier ? "org.ladybird.Ladybird.Development",
  extraArgs ? [ ],
  extraEnv ? { },
}:

let
  envExports = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (name: value: "export ${name}=${lib.escapeShellArg (toString value)}") extraEnv
  );

  argsArray = lib.concatStringsSep " " (map lib.escapeShellArg extraArgs);

  resolvedLocalAppPath = if localAppPath != null then localAppPath else "";

  resolvedUnwrapped =
    if unwrapped == null then
      ""
    else if stdenv.hostPlatform.isDarwin then
      "${unwrapped}"
    else
      "${unwrapped}/bin/Ladybird";

  launcherScript = ''
    #!${bash}/bin/bash
    set -euo pipefail

    ${envExports}

    default_args=(${argsArray})

    resolve_binary() {
      local candidate=""
      if [ -n "''${DOGGIRL_APP_PATH:-}" ]; then
        candidate="''${DOGGIRL_APP_PATH}"
      elif [ -n "''${LADYBIRD_APP_PATH:-}" ]; then
        candidate="''${LADYBIRD_APP_PATH}"
      elif [ -n ${lib.escapeShellArg resolvedLocalAppPath} ]; then
        candidate=${lib.escapeShellArg resolvedLocalAppPath}
      elif [ -n ${lib.escapeShellArg resolvedUnwrapped} ]; then
        candidate=${lib.escapeShellArg resolvedUnwrapped}
      fi

      if [ -n "$candidate" ]; then
        if [ -x "$candidate/Contents/MacOS/Ladybird" ]; then
          printf '%s\n' "$candidate/Contents/MacOS/Ladybird"
          return 0
        elif [ -x "$candidate/Applications/Ladybird.app/Contents/MacOS/Ladybird" ]; then
          printf '%s\n' "$candidate/Applications/Ladybird.app/Contents/MacOS/Ladybird"
          return 0
        elif [ -x "$candidate/bundle/Ladybird.app/Contents/MacOS/Ladybird" ]; then
          printf '%s\n' "$candidate/bundle/Ladybird.app/Contents/MacOS/Ladybird"
          return 0
        elif [ -x "$candidate/bin/Ladybird" ]; then
          printf '%s\n' "$candidate/bin/Ladybird"
          return 0
        elif [ -x "$candidate" ] && [ ! -d "$candidate" ]; then
          printf '%s\n' "$candidate"
          return 0
        fi
        echo "doggirl: configured browser binary not found under '$candidate'" >&2
        return 1
      fi

      for probe in \
        "$PWD/Build/distribution/bin/Ladybird.app/Contents/MacOS/Ladybird" \
        "$PWD/Build/release/bin/Ladybird.app/Contents/MacOS/Ladybird" \
        "$PWD/Build/debug/bin/Ladybird.app/Contents/MacOS/Ladybird" \
        "$PWD/Build/distribution/bin/Ladybird" \
        "$PWD/Build/release/bin/Ladybird" \
        "$PWD/Build/debug/bin/Ladybird" \
        "/Applications/Ladybird.app/Contents/MacOS/Ladybird" \
        "$HOME/Applications/Ladybird.app/Contents/MacOS/Ladybird"; do
        if [ -x "$probe" ]; then
          printf '%s\n' "$probe"
          return 0
        fi
      done

      cat >&2 <<'EOF'
    doggirl: could not locate a built Ladybird/doggirl executable.
    Set programs.doggirl.localAppPath or programs.doggirl.unwrappedPackage, or export
    DOGGIRL_APP_PATH=/path/to/Ladybird.app before launching.
    EOF
      return 1
    }

    target_bin="$(resolve_binary)"
    exec "$target_bin" "''${default_args[@]}" "$@"
  '';

  infoPlist = ''
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
      <dict>
        <key>CFBundleDevelopmentRegion</key>
        <string>en</string>
        <key>CFBundleDisplayName</key>
        <string>${appName}</string>
        <key>CFBundleExecutable</key>
        <string>${appName}</string>
        <key>CFBundleIconFile</key>
        <string>app_icon.icns</string>
        <key>CFBundleIdentifier</key>
        <string>${bundleIdentifier}</string>
        <key>CFBundleInfoDictionaryVersion</key>
        <string>6.0</string>
        <key>CFBundleName</key>
        <string>${appName}</string>
        <key>CFBundlePackageType</key>
        <string>APPL</string>
        <key>CFBundleShortVersionString</key>
        <string>0.1.0</string>
        <key>CFBundleVersion</key>
        <string>0.1.0</string>
        <key>NSHighResolutionCapable</key>
        <true/>
        <key>CFBundleDocumentTypes</key>
        <array>
          <dict>
            <key>CFBundleTypeName</key>
            <string>Web documents</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSItemContentTypes</key>
            <array>
              <string>public.html</string>
              <string>public.xhtml</string>
              <string>public.svg-image</string>
            </array>
          </dict>
        </array>
        <key>CFBundleURLTypes</key>
        <array>
          <dict>
            <key>CFBundleURLName</key>
            <string>Web site URL</string>
            <key>CFBundleURLSchemes</key>
            <array>
              <string>http</string>
              <string>https</string>
            </array>
          </dict>
          <dict>
            <key>CFBundleURLName</key>
            <string>Local file URL</string>
            <key>CFBundleURLSchemes</key>
            <array>
              <string>file</string>
            </array>
          </dict>
        </array>
      </dict>
    </plist>
  '';

  iconsetDir = "${repoRoot}/UI/Icons/macos/app_icon.iconset";
in
runCommand "doggirl-wrapped-0.1.0"
  {
    passAsFile = [
      "launcherScript"
      "infoPlist"
    ];
    inherit launcherScript infoPlist;
    nativeBuildInputs = lib.optionals stdenv.hostPlatform.isDarwin [ libicns ];
    meta = {
      description = "Declarative launcher and bundle wrapper for doggirl (Ladybird fork)";
      homepage = "https://github.com/dappermint/doggirl";
      license = lib.licenses.bsd2;
      platforms = lib.platforms.unix;
      mainProgram = "doggirl";
    };
  }
  ''
    mkdir -p "$out/bin"
    cp "$launcherScriptPath" "$out/bin/doggirl"
    chmod 0755 "$out/bin/doggirl"
    ln -s doggirl "$out/bin/ladybird"

    ${lib.optionalString stdenv.hostPlatform.isDarwin ''
      app_dir="$out/Applications/${appName}.app"
      mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
      cp "$launcherScriptPath" "$app_dir/Contents/MacOS/${appName}"
      chmod 0755 "$app_dir/Contents/MacOS/${appName}"
      cp "$infoPlistPath" "$app_dir/Contents/Info.plist"

      png2icns "$app_dir/Contents/Resources/app_icon.icns" \
        "${iconsetDir}/icon_16x16.png" \
        "${iconsetDir}/icon_32x32.png" \
        "${iconsetDir}/icon_128x128.png" \
        "${iconsetDir}/icon_256x256.png" \
        "${iconsetDir}/icon_512x512.png"
    ''}

    ${lib.optionalString stdenv.hostPlatform.isLinux ''
        mkdir -p "$out/share/applications" "$out/share/icons/hicolor/scalable/apps"
        cp "${repoRoot}/Meta/CMake/freedesktop/org.ladybird.Ladybird.svg" \
          "$out/share/icons/hicolor/scalable/apps/org.ladybird.Ladybird.svg"
        cat > "$out/share/applications/doggirl.desktop" <<EOF
      [Desktop Entry]
      Name=${appName}
      GenericName=Web Browser
      Comment=Independent web browser (Ladybird hard fork)
      Exec=$out/bin/doggirl %U
      Icon=org.ladybird.Ladybird
      Terminal=false
      Type=Application
      Categories=Network;WebBrowser;
      MimeType=text/html;application/xhtml+xml;x-scheme-handler/http;x-scheme-handler/https;
      EOF
    ''}
  ''
