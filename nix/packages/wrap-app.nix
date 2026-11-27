{
  lib,
  stdenv,
  runCommand,
  bash,
  python3,
  repoRoot,
  unwrapped ? null,
  localAppPath ? null,
  appName ? "Doggirl",
  bundleIdentifier ? "dev.dappermint.doggirl",
  extraArgs ? [ ],
  extraEnv ? { },
}:

let
  resolvedVersion =
    if unwrapped != null && unwrapped ? version then
      unwrapped.version
    else
      "0.3.0";

  passthroughDarwinApp =
    stdenv.hostPlatform.isDarwin
    && unwrapped != null
    && localAppPath == null
    && extraArgs == [ ]
    && extraEnv == { }
    && appName == "Doggirl"
    && bundleIdentifier == "dev.dappermint.doggirl";

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
      "${unwrapped}/bin/Doggirl";

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
        if [ -x "$candidate/Contents/MacOS/Doggirl" ]; then
          printf '%s\n' "$candidate/Contents/MacOS/Doggirl"
          return 0
        elif [ -x "$candidate/Applications/Doggirl.app/Contents/MacOS/Doggirl" ]; then
          printf '%s\n' "$candidate/Applications/Doggirl.app/Contents/MacOS/Doggirl"
          return 0
        elif [ -x "$candidate/bundle/Doggirl.app/Contents/MacOS/Doggirl" ]; then
          printf '%s\n' "$candidate/bundle/Doggirl.app/Contents/MacOS/Doggirl"
          return 0
        elif [ -x "$candidate/bin/Doggirl" ]; then
          printf '%s\n' "$candidate/bin/Doggirl"
          return 0
        elif [ -x "$candidate/Contents/MacOS/Ladybird" ]; then
          printf '%s\n' "$candidate/Contents/MacOS/Ladybird"
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
        "$PWD/Build/distribution/bin/Doggirl.app/Contents/MacOS/Doggirl" \
        "$PWD/Build/release/bin/Doggirl.app/Contents/MacOS/Doggirl" \
        "$PWD/Build/debug/bin/Doggirl.app/Contents/MacOS/Doggirl" \
        "$PWD/Build/distribution/bin/Doggirl" \
        "$PWD/Build/release/bin/Doggirl" \
        "$PWD/Build/debug/bin/Doggirl" \
        "/Applications/Doggirl.app/Contents/MacOS/Doggirl" \
        "$HOME/Applications/Doggirl.app/Contents/MacOS/Doggirl"; do
        if [ -x "$probe" ]; then
          printf '%s\n' "$probe"
          return 0
        fi
      done

      cat >&2 <<'EOF'
    doggirl: could not locate a built Doggirl executable.
    Set programs.doggirl.localAppPath or programs.doggirl.unwrappedPackage, or export
    DOGGIRL_APP_PATH=/path/to/Doggirl.app before launching.
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
        <key>CFBundleIconName</key>
        <string>AppIcon</string>
        <key>CFBundleIdentifier</key>
        <string>${bundleIdentifier}</string>
        <key>CFBundleInfoDictionaryVersion</key>
        <string>6.0</string>
        <key>CFBundleName</key>
        <string>${appName}</string>
        <key>CFBundlePackageType</key>
        <string>APPL</string>
        <key>CFBundleShortVersionString</key>
        <string>${resolvedVersion}</string>
        <key>CFBundleVersion</key>
        <string>${resolvedVersion}</string>
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
runCommand "doggirl-wrapped-${resolvedVersion}"
  {
    passAsFile = [
      "launcherScript"
      "infoPlist"
    ];
    inherit launcherScript infoPlist;
    nativeBuildInputs = lib.optionals stdenv.hostPlatform.isDarwin [ python3 ];
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

    ${lib.optionalString stdenv.hostPlatform.isDarwin (
      if passthroughDarwinApp then
        ''
          if [ -d "${unwrapped}/Applications/Doggirl.app" ]; then
            mkdir -p "$out/Applications"
            cp -a "${unwrapped}/Applications/Doggirl.app" "$out/Applications/Doggirl.app"
          else
            app_dir="$out/Applications/${appName}.app"
            mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
            cp "$launcherScriptPath" "$app_dir/Contents/MacOS/${appName}"
            chmod 0755 "$app_dir/Contents/MacOS/${appName}"
            cp "$infoPlistPath" "$app_dir/Contents/Info.plist"
          fi
        ''
      else
        ''
          app_dir="$out/Applications/${appName}.app"
          mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
          cp "$launcherScriptPath" "$app_dir/Contents/MacOS/${appName}"
          chmod 0755 "$app_dir/Contents/MacOS/${appName}"
          cp "$infoPlistPath" "$app_dir/Contents/Info.plist"

          if [ -n ${lib.escapeShellArg resolvedUnwrapped} ] && [ -f "${resolvedUnwrapped}/Applications/Doggirl.app/Contents/Resources/Assets.car" ]; then
            cp "${resolvedUnwrapped}/Applications/Doggirl.app/Contents/Resources/Assets.car" \
              "$app_dir/Contents/Resources/Assets.car"
          fi

          python3 - "${iconsetDir}" "$app_dir/Contents/Resources/app_icon.icns" <<'PY'
          import os
          import struct
          import sys

          iconset_dir, output_path = sys.argv[1], sys.argv[2]
          entries = [
              (b"icp4", "icon_16x16.png"),
              (b"icp5", "icon_32x32.png"),
              (b"ic11", "icon_16x16@2x.png"),
              (b"ic12", "icon_32x32@2x.png"),
              (b"ic07", "icon_128x128.png"),
              (b"ic13", "icon_128x128@2x.png"),
              (b"ic08", "icon_256x256.png"),
              (b"ic14", "icon_256x256@2x.png"),
              (b"ic09", "icon_512x512.png"),
              (b"ic10", "icon_512x512@2x.png"),
          ]
          chunks = []
          for icon_type, filename in entries:
              with open(os.path.join(iconset_dir, filename), "rb") as f:
                  data = f.read()
              chunks.append(icon_type + struct.pack(">I", len(data) + 8) + data)
          body = b"".join(chunks)
          with open(output_path, "wb") as f:
              f.write(b"icns" + struct.pack(">I", len(body) + 8) + body)
          PY
        ''
    )}

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
