{
  lib,
  stdenvNoCC,
  fetchurl,
  version ? "0.4.1",
  hash ? "sha256-npEXZEso4gnkCAQTCgPeToVFpjSrdUSMNxu/mlp2AKg=",
}:

stdenvNoCC.mkDerivation {
  pname = "doggirl-unwrapped";
  inherit version;

  src = fetchurl {
    url = "https://github.com/dappermint/doggirl/releases/download/v${version}/Doggirl-${version}-arm64.tar.gz";
    inherit hash;
  };

  sourceRoot = ".";
  dontConfigure = true;
  dontBuild = true;
  # Preserve the bundle's ad-hoc Mach-O code signatures and @executable_path rpaths untouched.
  dontFixup = true;

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/Applications" "$out/bin"
    cp -a Doggirl.app "$out/Applications/Doggirl.app"

    cat > "$out/bin/doggirl" <<EOF
    #!/bin/sh
    exec "$out/Applications/Doggirl.app/Contents/MacOS/Doggirl" "\$@"
    EOF
    chmod 0755 "$out/bin/doggirl"
    ln -s doggirl "$out/bin/ladybird"

    runHook postInstall
  '';

  meta = {
    description = "Doggirl web browser (prebuilt macOS Apple Silicon bundle)";
    homepage = "https://github.com/dappermint/doggirl";
    license = lib.licenses.bsd2;
    platforms = [ "aarch64-darwin" ];
    mainProgram = "doggirl";
  };
}
