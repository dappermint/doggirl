{ lib, self }:

let
  constants = import ./constants.nix;
  serializers = import ./serializers.nix { inherit lib constants; };
in
{
  inherit constants;
  inherit (constants)
    builtinSearchEngines
    builtinContentBlockerLists
    showBookmarksBarValues
    themePreferences
    colorThemes
    verticalTabsPositions
    tabProcessesValues
    autoplayPolicies
    permissionNames
    permissionPolicies
    permissionSiteDecisions
    crashReportSendingValues
    dnsModes
    dnsTypes
    userAgentPresets
    platformFeatures
    configVariables
    bundleIdentifiers
    ;
  inherit (serializers)
    deterministicUuid
    mkSettingsAttrset
    mkPermissionsAttrset
    mkBookmarksAttrset
    mkLibjsJitEnv
    mkLaunchArgs
    mkLaunchEnv
    mkProfileManifestEntry
    ;

  mkDoggirlReleaseBin = pkgs: args: pkgs.callPackage ../packages/release-bin.nix args;

  mkDoggirlWrapper =
    pkgs: args: pkgs.callPackage ../packages/wrap-app.nix ({ repoRoot = self; } // args);

  mkDoggirlFromSource =
    pkgs: args: pkgs.callPackage ../packages/from-source.nix ({ src = self; } // args);
}
