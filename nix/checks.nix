{
  pkgs,
  lib,
  self,
  doggirlLib,
  darwinModule,
  homeManagerModule,
  doggirlConfigApply,
}:

let
  # Minimal mock nix-darwin module environment so darwinModules.default can be
  # evaluated cleanly inside `nix flake check` without fetching nix-darwin.
  mockDarwinBase = {
    options = {
      assertions = lib.mkOption {
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              assertion = lib.mkOption { type = lib.types.bool; };
              message = lib.mkOption { type = lib.types.str; };
            };
          }
        );
        default = [ ];
      };
      system = {
        primaryUser = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
        };
        defaults.CustomUserPreferences = lib.mkOption {
          type = lib.types.attrsOf (lib.types.attrsOf lib.types.anything);
          default = { };
        };
        activationScripts.postActivation.text = lib.mkOption {
          type = lib.types.lines;
          default = "";
        };
      };
      environment.systemPackages = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
      };
      users.users = lib.mkOption {
        type = lib.types.attrsOf (
          lib.types.submodule {
            options.home = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
            };
          }
        );
        default = { };
      };
    };
  };

  mockHomeManagerBase = {
    options = {
      assertions = lib.mkOption {
        type = lib.types.listOf (
          lib.types.submodule {
            options = {
              assertion = lib.mkOption { type = lib.types.bool; };
              message = lib.mkOption { type = lib.types.str; };
            };
          }
        );
        default = [ ];
      };
      home = {
        homeDirectory = lib.mkOption {
          type = lib.types.str;
          default = "/Users/tester";
        };
        packages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
        };
        activation = lib.mkOption {
          type = lib.types.attrsOf lib.types.anything;
          default = { };
        };
      };
      xdg.configHome = lib.mkOption {
        type = lib.types.str;
        default = "/Users/tester/.config";
      };
      xdg.dataHome = lib.mkOption {
        type = lib.types.str;
        default = "/Users/tester/.local/share";
      };
    };
  };

  # Comprehensive configuration exercising every typed option across multiple profiles.
  comprehensiveProfile = {
    firstRunComplete = true;
    newTabPageURL = "https://example.org/start";
    restorePreviousSession = true;
    window = {
      position = {
        x = 120;
        y = 80;
      };
      size = {
        width = 1440;
        height = 960;
      };
      maximized = false;
    };
    languages = [
      "en-GB"
      "en"
    ];
    translationPrefersHigherQuality = true;
    appearance = {
      showAllTabsButton = false;
      showSidebarButton = false;
      showMenuBar = true;
      alwaysShowScrollbars = true;
      showBookmarksBar = "onHover";
      interfaceTheme = "dark";
      websiteColorScheme = "dark";
      colorTheme = "lavender";
    };
    content = {
      defaultZoomLevelFactor = 1.1;
      zoomPerHost = {
        "example.com" = 1.25;
        "docs.example.org" = 0.9;
      };
      enableForceDark = false;
      forceDarkPerHost = {
        "light.example.org" = true;
        "already-dark.example.org" = false;
      };
    };
    tabs = {
      verticalTabsEnabled = true;
      verticalTabsExpanded = true;
      verticalTabsExpandOnHover = true;
      verticalTabsPosition = "left";
      verticalTabsExpandedWidth = 260;
      everythingInSidebar = false;
      discardHiddenTabsAfterMinutes = 60;
      tabProcesses = "perSite";
    };
    browsingBehavior = {
      enableAutoscroll = true;
      enablePrimaryPaste = false;
    };
    searchEngine = {
      engine = "SearXNG";
      suggestions = true;
      custom = [
        {
          name = "SearXNG";
          url = "https://search.example.org/search?q=%s";
          suggestionsUrl = "https://search.example.org/autocompleter?q=%s";
        }
        {
          name = "Nixpkgs";
          url = "https://search.nixos.org/packages?query=%s";
        }
      ];
    };
    autoplay = {
      policy = "block-audio-and-video";
      siteFilters = {
        "https://media.example.org" = "allow-audio-and-video";
        "https://news.example.org" = "block-audio";
      };
    };
    permissions = {
      geolocation = {
        policy = "block";
        sites = {
          "https://maps.example.org" = "allow";
        };
      };
      microphone = {
        policy = "ask";
        sites = {
          "https://meet.example.org" = "allow";
          "https://untrusted.example.org" = "block";
        };
      };
      notifications = {
        policy = "block";
        sites = {
          "https://chat.example.org" = "allow";
        };
      };
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
      disabledSites = [ "trusted.example.org" ];
      builtInLists = {
        easyList = true;
        easyPrivacy = true;
        easyListCookie = true;
        fanboyAnnoyances = true;
        adblockWarningRemoval = true;
      };
      customSubscriptions = [
        {
          identifier = "custom-ublock";
          url = "https://filters.example.org/list.txt";
          enabled = true;
        }
      ];
      localLists = [
        {
          identifier = "local-blocklist";
          name = "Local Overrides";
          enabled = true;
          text = ''
            ||ads.example.org^
            @@||cdn.example.org^
          '';
        }
      ];
      customFilters = ''
        ||tracker.example.org^
      '';
    };
    dns = {
      mode = "custom";
      server = "1.1.1.1";
      type = "tls";
      dnssec = true;
    };
    configVariables = {
      showWebContentProcessId = true;
      showAdvancedDebugMenu = true;
      showTabPerformanceMonitor = true;
      showFeedbackButton = false;
      useClientSideWindowDecorations = true;
      verticalTabsHoverExpandDelayMs = 50;
      verticalTabsHoverExpandAnimationDurationMs = 75;
      maximumDiskCacheSizeMb = 2048;
      maximumConnectionsPerDownload = 6;
      splitDownloadsWithoutValidators = false;
      restartStalledConnections = true;
      disableHighDynamicRangeSurfaces = false;
      offerToTrustSelfSignedCertificates = true;
      updateCheckIntervalMinutes = 120;
    };
    bookmarks = [
      {
        title = "Ladybird";
        url = "https://ladybird.org/";
      }
      {
        title = "Development";
        children = [
          {
            title = "doggirl";
            url = "https://github.com/dappermint/doggirl";
          }
          {
            title = "Subfolder";
            children = [
              {
                id = "custom-stable-id";
                title = "Example";
                url = "https://example.org/";
              }
            ];
          }
        ];
      }
    ];
    extraSettings = {
      "x-custom-test-key" = {
        enabled = true;
      };
    };
  };

  minimalProfile = {
    firstRunComplete = true;
    searchEngine.engine = "DuckDuckGo";
    dns = {
      mode = "custom";
      server = "9.9.9.9";
      type = "udp";
      dnssec = false;
    };
  };

  darwinEval = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      mockDarwinBase
      darwinModule
      {
        system.primaryUser = "tester";
        users.users.tester.home = "/Users/tester";
        programs.doggirl = {
          enable = true;
          localAppPath = "/Applications/Doggirl.app";
          appName = "Doggirl";
          defaultProfile = "developer";
          manageMode = "overwrite";
          launch = {
            userAgentPreset = "Safari macOS Desktop";
            disableFeatures = [ "vulkan_video" ];
            forceEnableFeatures = [ "gpu_painting" ];
            devtoolsPort = 6000;
            webContentMemoryLimitMiB = 4096;
            maxSiteRequestServers = 8;
            exposeExperimentalInterfaces = true;
            logAllJsExceptions = true;
            defaultTimeZone = "Europe/London";
            dnsOverride = {
              server = "1.1.1.1";
              port = 853;
              dot = true;
              dnssec = true;
            };
            jit = {
              enable = true;
              threshold = 300;
              warmup = 4;
              compileThreads = 4;
              inlineBudget = 300;
              inlineMaxSize = 40;
              inlineDepth = 6;
            };
            codeFlush = "on";
            gc = {
              logLevel = "info";
              compact = true;
            };
            extraEnv.CUSTOM_FLAG = "enabled";
            extraArgs = [ "--new-window" ];
          };
          macosDefaults = {
            windowDragOnGesture = true;
            disableSparkleAutoChecks = true;
            quitAlwaysKeepsWindows = false;
          };
          profiles = {
            developer = comprehensiveProfile;
            default = minimalProfile;
          };
          customPathProfiles.isolated = {
            path = "/tmp/doggirl-custom-profile";
            firstRunComplete = true;
            appearance.interfaceTheme = "light";
          };
        };
      }
    ];
  };

  homeManagerEval = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      mockHomeManagerBase
      homeManagerModule
      {
        programs.doggirl = {
          enable = true;
          defaultProfile = "developer";
          manageMode = "merge";
          profiles.developer = comprehensiveProfile;
        };
      }
    ];
  };

  failedAssertions = eval: map (a: a.message) (lib.filter (a: !a.assertion) eval.config.assertions);

  darwinFailures = failedAssertions darwinEval;
  hmFailures = failedAssertions homeManagerEval;

  constantsJson = pkgs.writeText "doggirl-constants.json" (builtins.toJSON doggirlLib.constants);

  wrapperForSmoke = doggirlLib.mkDoggirlWrapper pkgs {
    localAppPath = "@MOCK_APP@";
    extraArgs = darwinEval.config.programs.doggirl.generated.launchArgs;
    extraEnv = darwinEval.config.programs.doggirl.generated.launchEnv;
  };
in
{
  darwin-module-eval =
    assert lib.assertMsg (
      darwinFailures == [ ]
    ) "darwin module assertions failed: ${lib.concatStringsSep "; " darwinFailures}";
    assert lib.assertMsg (
      builtins.length darwinEval.config.environment.systemPackages == 1
    ) "expected 1 system package";
    assert lib.assertMsg (
      darwinEval.config.system.defaults.CustomUserPreferences."dev.dappermint.doggirl".NSWindowShouldDragOnGesture
      == true
    ) "expected macOS default NSWindowShouldDragOnGesture = true";
    assert lib.assertMsg (
      darwinEval.config.system.defaults.CustomUserPreferences."dev.dappermint.doggirl.Development".SUEnableAutomaticChecks
      == false
    ) "expected Sparkle auto checks disabled";
    assert lib.assertMsg
      (lib.hasInfix "doggirl-config-apply" darwinEval.config.system.activationScripts.postActivation.text)
      "expected postActivation hook to invoke doggirl-config-apply";
    pkgs.runCommand "check-darwin-module-eval" { } ''
      cp ${darwinEval.config.programs.doggirl.generated.manifestFile} "$out"
    '';

  home-manager-module-eval =
    assert lib.assertMsg (
      hmFailures == [ ]
    ) "home-manager module assertions failed: ${lib.concatStringsSep "; " hmFailures}";
    assert lib.assertMsg (
      builtins.length homeManagerEval.config.home.packages == 1
    ) "expected 1 home package";
    pkgs.runCommand "check-home-manager-module-eval" { } ''
      cp ${homeManagerEval.config.programs.doggirl.generated.manifestFile} "$out"
    '';

  schema-validation =
    pkgs.runCommand "check-schema-validation"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - \
          "${self}" \
          "${constantsJson}" \
          "${darwinEval.config.programs.doggirl.generated.manifestFile}" <<'PY'
        import json
        import re
        import sys
        from pathlib import Path

        repo = Path(sys.argv[1])
        constants = json.loads(Path(sys.argv[2]).read_text())
        manifest = json.loads(Path(sys.argv[3]).read_text())

        settings_cpp = (repo / "Libraries/LibWebView/Settings.cpp").read_text()
        settings_h = (repo / "Libraries/LibWebView/Settings.h").read_text()
        search_cpp = (repo / "Libraries/LibWebView/SearchEngine.cpp").read_text()
        perm_cpp = (repo / "Libraries/LibWebCommon/Permissions/Permissions.cpp").read_text()
        autoplay_cpp = (repo / "Libraries/LibWebCommon/HTML/AutoplayPolicy.cpp").read_text()
        ua_cpp = (repo / "Libraries/LibWebView/UserAgent.cpp").read_text()
        feature_cpp = (repo / "Libraries/LibPlatformFeatures/Feature.cpp").read_text()

        # 1. Built-in search engines match SearchEngine.cpp exactly.
        cpp_engines = re.findall(r'\{\s*"([^"]+)"_string,\s*"https?://', search_cpp)
        assert sorted(cpp_engines) == sorted(constants["builtinSearchEngines"]), (
            f"search engine mismatch: cpp={cpp_engines} nix={constants['builtinSearchEngines']}"
        )

        # 2. Built-in content blocker lists match Settings.cpp add_list() calls exactly.
        cpp_lists = re.findall(r'add_list\("([^"]+)"sv,', settings_cpp)
        assert sorted(cpp_lists) == sorted(constants["builtinContentBlockerLists"]), (
            f"content blocker lists mismatch: cpp={cpp_lists} nix={constants['builtinContentBlockerLists']}"
        )

        # 3. Colour themes match ENUMERATE_COLOR_THEMES in Settings.h.
        cpp_themes = re.findall(r'X\(\w+,\s*"([^"]+)"\)', settings_h)
        assert sorted(cpp_themes) == sorted(constants["colorThemes"]), (
            f"colour themes mismatch: cpp={cpp_themes} nix={constants['colorThemes']}"
        )

        # 4. Permission names match Permissions.cpp.
        cpp_perms = re.findall(r'return "([^"]+)"sv;', perm_cpp)
        assert sorted(cpp_perms) == sorted(constants["permissionNames"]), (
            f"permission names mismatch: cpp={cpp_perms} nix={constants['permissionNames']}"
        )

        # 5. Autoplay policies match AutoplayPolicy.cpp.
        cpp_autoplay = re.findall(r'return u"([^"]+)"sv;', autoplay_cpp)
        assert sorted(cpp_autoplay) == sorted(constants["autoplayPolicies"]), (
            f"autoplay policies mismatch: cpp={cpp_autoplay} nix={constants['autoplayPolicies']}"
        )

        # 6. User-Agent presets match UserAgent.cpp.
        cpp_uas = re.findall(r'\{\s*"([^"]+)"sv,\s*"Mozilla/', ua_cpp)
        assert sorted(cpp_uas) == sorted(constants["userAgentPresets"]), (
            f"user agent presets mismatch: cpp={cpp_uas} nix={constants['userAgentPresets']}"
        )

        # 7. Platform feature IDs match LibPlatformFeatures/Feature.cpp.
        cpp_features = re.findall(r'case Feature::\w+:\s+return "([^"]+)"sv;', feature_cpp)
        assert sorted(cpp_features) == sorted(constants["platformFeatures"]), (
            f"platform features mismatch: cpp={cpp_features} nix={constants['platformFeatures']}"
        )

        # 8. Every CONFIG_VARIABLE_DEFINITIONS .name in Settings.cpp is covered in constants.
        cpp_config_vars = re.findall(r'\.name\s*=\s*"([^"]+)"sv,', settings_cpp)
        nix_config_vars = [v["key"] for v in constants["configVariables"].values()]
        assert sorted(cpp_config_vars) == sorted(nix_config_vars), (
            f"configVariables mismatch: cpp={cpp_config_vars} nix={nix_config_vars}"
        )

        # 9. Validate generated developer profile Settings.json, Permissions.json, Bookmarks.json.
        dev = next(entry for entry in manifest if entry["name"] == "developer")
        settings = json.loads(Path(dev["settingsFile"]).read_text())

        expected_top_keys = {
            "firstRunComplete",
            "newTabPageURL",
            "restorePreviousSession",
            "window",
            "languages",
            "translationPrefersHigherQuality",
            "appearance",
            "content",
            "tabs",
            "browsingBehavior",
            "searchEngine",
            "autoplay",
            "geolocation",
            "microphone",
            "notifications",
            "globalPrivacyControl",
            "crashReportSending",
            "backgroundNetworking",
            "contentBlockers",
            "dnsSettings",
            "configVariables",
            "x-custom-test-key",
        }
        assert set(settings.keys()) == expected_top_keys, (
            f"top-level key mismatch: {set(settings.keys()) ^ expected_top_keys}"
        )

        assert settings["firstRunComplete"] is True
        assert settings["window"] == {
            "position": {"x": 120, "y": 80},
            "size": {"width": 1440, "height": 960},
            "maximized": False,
        }
        assert settings["languages"] == ["en-GB", "en"]
        assert settings["appearance"] == {
            "showAllTabsButton": False,
            "showSidebarButton": False,
            "showMenuBar": True,
            "alwaysShowScrollbars": True,
            "showBookmarksBar": "onHover",
            "interfaceTheme": "dark",
            "websiteColorScheme": "dark",
            "colorTheme": "lavender",
        }

        # Verify every *_KEY constant declared in Settings.cpp is represented in our generated schema.
        cpp_keys = set(re.findall(r'static constexpr auto \w+_KEY = "([^"]+)"sv;', settings_cpp))
        present_keys = set()
        def collect_keys(obj):
            if isinstance(obj, dict):
                for k, v in obj.items():
                    present_keys.add(k)
                    collect_keys(v)
            elif isinstance(obj, list):
                for item in obj:
                    collect_keys(item)
        collect_keys(settings)
        missing_keys = cpp_keys - present_keys
        assert not missing_keys, f"Settings.cpp keys missing from generated JSON: {sorted(missing_keys)}"
        assert settings["content"]["defaultZoomLevelFactor"] == 1.1
        assert settings["content"]["zoomPerHost"] == {
            "docs.example.org": 0.9,
            "example.com": 1.25,
        }
        assert settings["tabs"]["verticalTabsExpandedWidth"] == 260
        assert settings["tabs"]["tabProcesses"] == "perSite"
        assert settings["searchEngine"]["engine"] == "SearXNG"
        assert len(settings["searchEngine"]["custom"]) == 2
        assert settings["autoplay"]["policy"] == "block-audio-and-video"
        assert len(settings["autoplay"]["siteFilters"]) == 2
        assert settings["geolocation"] == {"policy": "block"}
        assert settings["microphone"] == {"policy": "ask"}
        assert settings["notifications"] == {"policy": "block"}
        assert settings["globalPrivacyControl"] is True
        assert settings["crashReportSending"] == "never"
        assert settings["backgroundNetworking"] == {
            "enabled": True,
            "features": {
                "contentBlockerSubscriptionUpdates": True,
                "updateChecks": False,
            },
        }
        assert set(settings["contentBlockers"]["builtInLists"].keys()) == set(cpp_lists)
        assert settings["contentBlockers"]["builtInLists"]["easyList"] is True
        assert settings["contentBlockers"]["builtInLists"]["ruAdList"] is False
        assert settings["dnsSettings"] == {
            "mode": "custom",
            "server": "1.1.1.1",
            "port": 853,
            "type": "tls",
            "dnssec": True,
        }
        assert set(settings["configVariables"].keys()) == set(cpp_config_vars)
        assert settings["configVariables"]["updates.check_interval_minutes"] == 120
        assert settings["x-custom-test-key"] == {"enabled": True}

        # Verify UDP DNS default port (53) in minimal profile.
        default_profile = next(entry for entry in manifest if entry["name"] == "default")
        default_settings = json.loads(Path(default_profile["settingsFile"]).read_text())
        assert "window" not in default_settings
        assert "languages" not in default_settings
        assert default_settings["dnsSettings"]["port"] == 53
        assert default_settings["dnsSettings"]["type"] == "udp"

        # Validate Permissions.json (PermissionManager.cpp v1 schema).
        perms = json.loads(Path(dev["permissionsFile"]).read_text())
        assert perms["version"] == 1
        assert perms["embargoes"] == []
        assert len(perms["decisions"]) == 4
        for decision in perms["decisions"]:
            assert set(decision.keys()) == {"origin", "permission", "decision"}
            assert decision["permission"] in constants["permissionNames"]
            assert decision["decision"] in constants["permissionSiteDecisions"]

        # Validate Bookmarks.json (BookmarkStore.cpp v2 schema).
        bookmarks = json.loads(Path(dev["bookmarksFile"]).read_text())
        assert bookmarks["version"] == 2
        assert len(bookmarks["items"]) == 2
        assert bookmarks["items"][0]["type"] == "bookmark"
        assert bookmarks["items"][0]["url"] == "https://ladybird.org/"
        folder = bookmarks["items"][1]
        assert folder["type"] == "folder"
        assert folder["title"] == "Development"
        assert folder["children"][1]["children"][0]["id"] == "custom-stable-id"

        print("schema-validation: all C++ constants and generated JSON files verified")
        PY
        touch "$out"
      '';

  config-apply-modes =
    pkgs.runCommand "check-config-apply-modes"
      {
        nativeBuildInputs = [
          doggirlConfigApply
          pkgs.jq
        ];
      }
      ''
        manifest="$TMPDIR/manifest.json"
        jq --arg custom "$TMPDIR/doggirl-custom-profile" \
          'map(if .customPath != null then .customPath = $custom else . end)' \
          "${darwinEval.config.programs.doggirl.generated.manifestFile}" > "$manifest"

        # 1. Overwrite mode + backup creation
        home_over="$TMPDIR/home-overwrite"
        cfg_over="$home_over/cfg"
        data_over="$home_over/data"
        mkdir -p "$cfg_over/Doggirl/Profiles/developer"
        printf '{"window":{"size":{"width":800,"height":600}},"extraOld":1}\n' \
          > "$cfg_over/Doggirl/Profiles/developer/Settings.json"

        doggirl-config-apply \
          --manifest "$manifest" \
          --home "$home_over" \
          --config-base "$cfg_over" \
          --data-base "$data_over" \
          --mode overwrite \
          --backup

        test -f "$cfg_over/Doggirl/Profiles/developer/Settings.json"
        test -f "$cfg_over/Doggirl/Profiles/developer/Settings.json.bak"
        test -f "$cfg_over/Doggirl/Profiles/developer/Permissions.json"
        test -f "$cfg_over/Doggirl/Profiles/developer/Bookmarks.json"
        test -f "$data_over/Doggirl/Profiles/developer/ContentBlocking/Lists/local-blocklist.txt"
        test -f "$cfg_over/Doggirl/Profiles/default/Settings.json"
        test ! -e "$cfg_over/Doggirl/Profiles/default/Permissions.json"
        test ! -e "$cfg_over/Doggirl/Profiles/default/Bookmarks.json"
        test -f "$TMPDIR/doggirl-custom-profile/config/Settings.json"
        jq -e '.extraOld == null and .appearance.colorTheme == "lavender"' \
          "$cfg_over/Doggirl/Profiles/developer/Settings.json" >/dev/null

        # 2. Merge mode preserves undeclared keys (such as existing runtime window geometry)
        home_merge="$TMPDIR/home-merge"
        cfg_merge="$home_merge/cfg"
        data_merge="$home_merge/data"
        mkdir -p "$cfg_merge/Doggirl/Profiles/default"
        printf '{"window":{"size":{"width":1600,"height":1000},"maximized":true},"runtimeOnly":42}\n' \
          > "$cfg_merge/Doggirl/Profiles/default/Settings.json"

        doggirl-config-apply \
          --manifest "$manifest" \
          --home "$home_merge" \
          --config-base "$cfg_merge" \
          --data-base "$data_merge" \
          --mode merge \
          --no-backup

        jq -e '.window.size.width == 1600 and .runtimeOnly == 42 and .searchEngine.engine == "DuckDuckGo"' \
          "$cfg_merge/Doggirl/Profiles/default/Settings.json" >/dev/null

        # 3. Symlink mode links directly into the Nix store
        home_sym="$TMPDIR/home-symlink"
        cfg_sym="$home_sym/cfg"
        data_sym="$home_sym/data"
        doggirl-config-apply \
          --manifest "$manifest" \
          --home "$home_sym" \
          --config-base "$cfg_sym" \
          --data-base "$data_sym" \
          --mode symlink \
          --no-backup

        test -L "$cfg_sym/Doggirl/Profiles/developer/Settings.json"
        test -L "$cfg_sym/Doggirl/Profiles/developer/Permissions.json"
        test -L "$cfg_sym/Doggirl/Profiles/developer/Bookmarks.json"
        test -L "$data_sym/Doggirl/Profiles/developer/ContentBlocking/Lists/local-blocklist.txt"

        touch "$out"
      '';

  wrapper-smoke =
    pkgs.runCommand "check-wrapper-smoke"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        mock_app="$TMPDIR/MockDoggirl.app"
        mkdir -p "$mock_app/Contents/MacOS"
        cat > "$mock_app/Contents/MacOS/Doggirl" <<'EOF'
        #!${pkgs.bash}/bin/bash
        printf 'LIBJS_JIT=%s\n' "''${LIBJS_JIT:-}"
        printf 'LIBJS_CODE_FLUSH=%s\n' "''${LIBJS_CODE_FLUSH:-}"
        printf 'LIBGC_LOG_LEVEL=%s\n' "''${LIBGC_LOG_LEVEL:-}"
        printf 'LIBGC_COMPACT=%s\n' "''${LIBGC_COMPACT:-}"
        printf 'CUSTOM_FLAG=%s\n' "''${CUSTOM_FLAG:-}"
        printf 'ARGS=%s\n' "$*"
        EOF
        chmod +x "$mock_app/Contents/MacOS/Doggirl"

        output="$(DOGGIRL_APP_PATH="$mock_app" "${wrapperForSmoke}/bin/doggirl" https://example.org)"
        printf '%s\n' "$output"

        printf '%s\n' "$output" | grep -q 'LIBJS_JIT=on,threshold=300,warmup=4,compile-threads=4,inline-budget=300,inline-max-size=40,inline-depth=6'
        printf '%s\n' "$output" | grep -q 'LIBJS_CODE_FLUSH=on'
        printf '%s\n' "$output" | grep -q 'LIBGC_LOG_LEVEL=info'
        printf '%s\n' "$output" | grep -q 'LIBGC_COMPACT=1'
        printf '%s\n' "$output" | grep -q 'CUSTOM_FLAG=enabled'
        printf '%s\n' "$output" | grep -q -- '--profile developer'
        printf '%s\n' "$output" | grep -q -- '--user-agent-preset Safari macOS Desktop'
        printf '%s\n' "$output" | grep -q -- '--disable-features vulkan_video'
        printf '%s\n' "$output" | grep -q -- '--force-enable-features gpu_painting'
        printf '%s\n' "$output" | grep -q -- '--devtools=6000'
        printf '%s\n' "$output" | grep -q -- '--web-content-memory-limit 4096'
        printf '%s\n' "$output" | grep -q -- '--dns-server 1.1.1.1 --dns-port 853 --dot --dnssec'
        printf '%s\n' "$output" | grep -q -- '--new-window https://example.org'

        ${lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
          test -f "${wrapperForSmoke}/Applications/Doggirl.app/Contents/Info.plist"
          test -f "${wrapperForSmoke}/Applications/Doggirl.app/Contents/Resources/app_icon.icns"
          test -x "${wrapperForSmoke}/Applications/Doggirl.app/Contents/MacOS/Doggirl"
        ''}

        touch "$out"
      '';

  nix-format =
    pkgs.runCommand "check-nix-format"
      {
        nativeBuildInputs = [ pkgs.nixfmt ];
      }
      ''
        nixfmt --check "${self}/flake.nix" $(find "${self}/nix" -name '*.nix' | sort)
        touch "$out"
      '';

  nix-lint =
    pkgs.runCommand "check-nix-lint"
      {
        nativeBuildInputs = [
          pkgs.statix
          pkgs.deadnix
        ];
      }
      ''
        statix check "${self}/flake.nix"
        statix check "${self}/nix"
        deadnix --fail "${self}/flake.nix" "${self}/nix"
        touch "$out"
      '';
}
