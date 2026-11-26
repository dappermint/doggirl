{
  doggirlLib,
}:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.doggirl;

  defaultConfigBase =
    if pkgs.stdenv.hostPlatform.isDarwin then
      "${config.home.homeDirectory}/Library/Preferences"
    else
      config.xdg.configHome or "${config.home.homeDirectory}/.config";

  defaultDataBase =
    if pkgs.stdenv.hostPlatform.isDarwin then
      "${config.home.homeDirectory}/Library/Application Support"
    else
      config.xdg.dataHome or "${config.home.homeDirectory}/.local/share";

  resolvedConfigBase = if cfg.configBaseDir != null then cfg.configBaseDir else defaultConfigBase;
  resolvedDataBase = if cfg.dataBaseDir != null then cfg.dataBaseDir else defaultDataBase;

  applyFlags = [
    "--manifest"
    (toString cfg.generated.manifestFile)
    "--home"
    config.home.homeDirectory
    "--config-base"
    resolvedConfigBase
    "--data-base"
    resolvedDataBase
    "--mode"
    cfg.manageMode
    (if cfg.backupExisting then "--backup" else "--no-backup")
  ];
in
{
  imports = [
    (import ./schema.nix { inherit doggirlLib; })
  ];

  config = lib.mkIf cfg.enable {
    home.packages = lib.optional (cfg.package != null) cfg.package;

    home.activation.doggirlConfig = lib.mkIf cfg.generated.hasProfiles (
      let
        cmd = "${lib.getExe cfg.generated.configApplyPackage} ${lib.escapeShellArgs applyFlags}";
      in
      if lib ? hm && lib.hm ? dag then
        lib.hm.dag.entryAfter [ "writeBoundary" ] ''
          run ${cmd}
        ''
      else
        cmd
    );
  };
}
