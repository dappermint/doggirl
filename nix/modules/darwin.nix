{
  doggirlLib,
}:
{
  config,
  lib,
  ...
}:

let
  inherit (lib) mkOption types;
  cfg = config.programs.doggirl;
  inherit (doggirlLib) constants;

  macosPrefs =
    lib.optionalAttrs (cfg.macosDefaults.windowDragOnGesture != null) {
      NSWindowShouldDragOnGesture = cfg.macosDefaults.windowDragOnGesture;
    }
    // lib.optionalAttrs (cfg.macosDefaults.disableSparkleAutoChecks == true) {
      SUEnableAutomaticChecks = false;
      SUAutomaticallyUpdate = false;
    }
    // lib.optionalAttrs (cfg.macosDefaults.disableSparkleAutoChecks == false) {
      SUEnableAutomaticChecks = true;
    }
    // lib.optionalAttrs (cfg.macosDefaults.quitAlwaysKeepsWindows != null) {
      NSQuitAlwaysKeepsWindows = cfg.macosDefaults.quitAlwaysKeepsWindows;
    }
    // cfg.macosDefaults.extra;

  hasMacosPrefs = cfg.macosDefaults.enable || macosPrefs != { };

  resolvedHome =
    if cfg.homeDirectory != null then
      cfg.homeDirectory
    else if
      cfg.user != null
      && config.users.users ? ${cfg.user}
      && (config.users.users.${cfg.user}.home or null) != null
    then
      config.users.users.${cfg.user}.home
    else if cfg.user != null then
      "/Users/${cfg.user}"
    else
      null;

  applyFlags = [
    "--manifest"
    (toString cfg.generated.manifestFile)
    "--home"
    (toString resolvedHome)
    "--mode"
    cfg.manageMode
    (if cfg.backupExisting then "--backup" else "--no-backup")
  ]
  ++ lib.optionals (cfg.configBaseDir != null) [
    "--config-base"
    cfg.configBaseDir
  ]
  ++ lib.optionals (cfg.dataBaseDir != null) [
    "--data-base"
    cfg.dataBaseDir
  ]
  ++ lib.optionals (cfg.user != null) [
    "--owner"
    cfg.user
    "--group"
    cfg.group
  ];
in
{
  imports = [
    (import ./schema.nix { inherit doggirlLib; })
  ];

  options.programs.doggirl = {
    user = mkOption {
      type = types.nullOr types.str;
      default = config.system.primaryUser or null;
      defaultText = lib.literalExpression "config.system.primaryUser or null";
      description = "macOS user account that owns the managed doggirl browser profiles.";
    };

    group = mkOption {
      type = types.str;
      default = "staff";
      description = "Primary macOS group applied to managed profile files during activation.";
    };

    homeDirectory = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Explicit home directory for `programs.doggirl.user`. Defaults to
        `config.users.users.<user>.home` or `/Users/<user>`.
      '';
    };

    macosDefaults = mkOption {
      type = types.submodule {
        options = {
          enable = mkOption {
            type = types.bool;
            default = false;
            description = "Write macOS NSUserDefaults preferences via `system.defaults.CustomUserPreferences`.";
          };

          bundleIdentifiers = mkOption {
            type = types.listOf types.str;
            default = [
              constants.bundleIdentifiers.release
              constants.bundleIdentifiers.development
            ];
            description = "macOS preference domains (CFBundleIdentifier) to receive these defaults.";
          };

          windowDragOnGesture = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Set `NSWindowShouldDragOnGesture` (read by `UI/Qt/MacWindow.mm`).";
          };

          disableSparkleAutoChecks = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Set Sparkle `SUEnableAutomaticChecks` / `SUAutomaticallyUpdate` defaults.";
          };

          quitAlwaysKeepsWindows = mkOption {
            type = types.nullOr types.bool;
            default = null;
            description = "Set `NSQuitAlwaysKeepsWindows` for AppKit window restoration.";
          };

          extra = mkOption {
            type = types.attrsOf types.anything;
            default = { };
            description = "Additional NSUserDefaults key/value pairs written under each bundle identifier.";
          };
        };
      };
      default = { };
      description = "macOS AppKit / Sparkle NSUserDefaults preferences for Ladybird/doggirl.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.generated.hasProfiles || (cfg.user != null && cfg.user != "");
        message = "programs.doggirl.user (or system.primaryUser) must be set when managing profiles on nix-darwin";
      }
      {
        assertion = !cfg.generated.hasProfiles || (resolvedHome != null && lib.hasPrefix "/" resolvedHome);
        message = "programs.doggirl.homeDirectory must resolve to an absolute path";
      }
    ];

    environment.systemPackages = lib.optional (cfg.package != null) cfg.package;

    system.defaults.CustomUserPreferences = lib.mkIf hasMacosPrefs (
      lib.genAttrs cfg.macosDefaults.bundleIdentifiers (_: macosPrefs)
    );

    system.activationScripts.postActivation.text = lib.mkIf cfg.generated.hasProfiles (
      lib.mkAfter ''
        echo "configuring doggirl browser profiles for ${cfg.user}..." >&2
        ${lib.getExe cfg.generated.configApplyPackage} ${lib.escapeShellArgs applyFlags}
      ''
    );
  };
}
