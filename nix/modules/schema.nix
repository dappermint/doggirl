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
  inherit (lib) mkOption mkEnableOption types;
  cfg = config.programs.doggirl;
  inherit (doggirlLib) constants;
  jsonFormat = pkgs.formats.json { };

  nonNegInt = types.ints.unsigned;
  posInt = types.ints.positive;
  min5Int = types.addCheck types.int (v: v >= 5) // {
    description = "integer greater than or equal to 5";
  };

  configVarTypeFor =
    typeName:
    if typeName == "bool" then
      types.bool
    else if typeName == "nonNegInt" then
      nonNegInt
    else if typeName == "posInt" then
      posInt
    else if typeName == "min5Int" then
      min5Int
    else
      throw "unknown config variable type '${typeName}'";

  bookmarkItemType = types.submodule {
    options = {
      type = mkOption {
        type = types.nullOr (
          types.enum [
            "bookmark"
            "folder"
          ]
        );
        default = null;
        description = ''
          Item kind ('bookmark' or 'folder'). Inferred automatically from
          whether `url` or `children` is set when left null.
        '';
      };

      id = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Stable bookmark item identifier. Generated deterministically from the
          item path, title and URL when left null.
        '';
      };

      title = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Display title for the bookmark or folder.";
      };

      url = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Destination URL for a bookmark item.";
      };

      faviconHash = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional favicon hash recorded in Bookmarks.json.";
      };

      dateAdded = mkOption {
        type = types.int;
        default = 0;
        description = "Creation timestamp in milliseconds since the Unix epoch.";
      };

      lastModified = mkOption {
        type = types.int;
        default = 0;
        description = "Last modification timestamp in milliseconds since the Unix epoch.";
      };

      children = mkOption {
        type = types.nullOr (types.listOf bookmarkItemType);
        default = null;
        description = "Nested bookmark items when this item is a folder.";
      };
    };
  };

  customSearchEngineType = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "Unique search engine display name.";
      };

      url = mkOption {
        type = types.str;
        description = "HTTP(S) query URL template containing '%s'.";
      };

      suggestionsUrl = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional HTTP(S) autocomplete suggestions URL template containing '%s'.";
      };
    };
  };

  customSubscriptionType = types.submodule {
    options = {
      identifier = mkOption {
        type = types.str;
        description = "List identifier (1 to 128 ASCII letters, digits or '-').";
      };

      url = mkOption {
        type = types.str;
        description = "HTTP(S) URL of the filter list subscription.";
      };

      enabled = mkOption {
        type = types.bool;
        default = true;
        description = "Whether this custom subscription is active.";
      };
    };
  };

  localContentBlockerListType = types.submodule {
    options = {
      identifier = mkOption {
        type = types.str;
        description = "List identifier (1 to 128 ASCII letters, digits or '-').";
      };

      name = mkOption {
        type = types.str;
        description = "Human-readable name for the local filter list.";
      };

      enabled = mkOption {
        type = types.bool;
        default = true;
        description = "Whether this local filter list is active.";
      };

      text = mkOption {
        type = types.nullOr types.lines;
        default = null;
        description = ''
          Inline filter rules written to
          `<dataDir>/ContentBlocking/Lists/<identifier>.txt` during activation.
        '';
      };

      source = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = ''
          Path to a filter list file installed to
          `<dataDir>/ContentBlocking/Lists/<identifier>.txt` during activation.
        '';
      };
    };
  };

  builtInListsType = types.submodule {
    freeformType = types.attrsOf types.bool;
    options = lib.genAttrs constants.builtinContentBlockerLists (
      id:
      mkOption {
        type = types.bool;
        default = false;
        description = "Enable the built-in '${id}' content blocker list.";
      }
    );
  };

  permissionSectionType =
    permName:
    types.submodule {
      options = {
        policy = mkOption {
          type = types.enum constants.permissionPolicies;
          default = "ask";
          description = "Default global policy for '${permName}' permission prompts.";
        };

        sites = mkOption {
          type = types.attrsOf (types.enum constants.permissionSiteDecisions);
          default = { };
          description = ''
            Per-origin persistent decisions ('allow' or 'block') for '${permName}',
            written to `Permissions.json`. Keys must be serialized origins such as
            `https://example.com`.
          '';
        };
      };
    };

  profileOptions = {
    firstRunComplete = mkOption {
      type = types.bool;
      default = false;
      description = "Skip the `about:welcome` onboarding flow when true.";
    };

    newTabPageURL = mkOption {
      type = types.str;
      default = "about:newtab";
      description = "URL opened in new tabs (`newTabPageURL`).";
    };

    restorePreviousSession = mkOption {
      type = types.bool;
      default = false;
      description = "Restore windows and tabs from the previous session at launch.";
    };

    window = mkOption {
      type = types.nullOr (
        types.submodule {
          options = {
            position = mkOption {
              type = types.nullOr (
                types.submodule {
                  options = {
                    x = mkOption {
                      type = types.int;
                      description = "Horizontal window coordinate in pixels.";
                    };
                    y = mkOption {
                      type = types.int;
                      description = "Vertical window coordinate in pixels.";
                    };
                  };
                }
              );
              default = null;
              description = "Optional saved window origin.";
            };

            size = mkOption {
              type = types.submodule {
                options = {
                  width = mkOption {
                    type = posInt;
                    default = 1200;
                    description = "Window width in pixels.";
                  };
                  height = mkOption {
                    type = posInt;
                    default = 900;
                    description = "Window height in pixels.";
                  };
                };
              };
              default = { };
              description = "Default window dimensions.";
            };

            maximized = mkOption {
              type = types.bool;
              default = false;
              description = "Start the window maximised.";
            };
          };
        }
      );
      default = null;
      description = ''
        Optional saved window geometry (`window`). Left null by default so the
        browser keeps its own window size and position.
      '';
    };

    languages = mkOption {
      type = types.nullOr (types.listOf types.str);
      default = null;
      example = [
        "en-GB"
        "en"
      ];
      description = ''
        Ordered BCP-47 preferred languages (`languages`). When null, the key is
        omitted so the browser detects platform languages automatically.
      '';
    };

    translationPrefersHigherQuality = mkOption {
      type = types.bool;
      default = false;
      description = "Prefer higher-quality platform translation models over faster ones.";
    };

    appearance = mkOption {
      type = types.submodule {
        options = {
          showMenuBar = mkOption {
            type = types.bool;
            default = false;
            description = "Show the application menu bar inside the browser window.";
          };

          alwaysShowScrollbars = mkOption {
            type = types.bool;
            default = false;
            description = "Always show scrollbars instead of overlay scrollbars.";
          };

          showBookmarksBar = mkOption {
            type = types.enum constants.showBookmarksBarValues;
            default = "never";
            description = "When to show the bookmarks bar ('always', 'never', or 'onHover').";
          };

          interfaceTheme = mkOption {
            type = types.enum constants.themePreferences;
            default = "system";
            description = "Theme preference for browser chrome ('system', 'light', or 'dark').";
          };

          websiteColorScheme = mkOption {
            type = types.enum constants.themePreferences;
            default = "system";
            description = "Preferred colour scheme advertised to websites ('system', 'light', or 'dark').";
          };

          colorTheme = mkOption {
            type = types.enum constants.colorThemes;
            default = "neutral";
            description = "Browser chrome colour theme palette.";
          };
        };
      };
      default = { };
      description = "Browser appearance settings (`appearance`).";
    };

    content = mkOption {
      type = types.submodule {
        options = {
          defaultZoomLevelFactor = mkOption {
            type = types.number;
            default = 1.0;
            description = "Default page zoom factor (1.0 = 100%).";
          };

          zoomPerHost = mkOption {
            type = types.attrsOf types.number;
            default = { };
            example = {
              "example.com" = 1.25;
            };
            description = "Per-host zoom factor overrides.";
          };

          enableForceDark = mkOption {
            type = types.bool;
            default = false;
            description = "Force dark rendering on light web pages by default.";
          };

          forceDarkPerHost = mkOption {
            type = types.attrsOf types.bool;
            default = { };
            example = {
              "example.com" = false;
            };
            description = "Per-host force dark overrides.";
          };
        };
      };
      default = { };
      description = "Page content display settings (`content`).";
    };

    tabs = mkOption {
      type = types.submodule {
        options = {
          verticalTabsEnabled = mkOption {
            type = types.bool;
            default = false;
            description = "Display tabs in a vertical sidebar.";
          };

          verticalTabsExpanded = mkOption {
            type = types.bool;
            default = true;
            description = "Whether the vertical tabs sidebar starts expanded.";
          };

          verticalTabsExpandOnHover = mkOption {
            type = types.bool;
            default = false;
            description = "Expand collapsed vertical tabs when hovering the sidebar.";
          };

          verticalTabsPosition = mkOption {
            type = types.enum constants.verticalTabsPositions;
            default = "left";
            description = "Side of the window for the vertical tabs sidebar ('left' or 'right').";
          };

          verticalTabsExpandedWidth = mkOption {
            type = types.nullOr types.ints.u16;
            default = null;
            description = "Optional width of the expanded vertical tabs sidebar in pixels (u16).";
          };

          everythingInSidebar = mkOption {
            type = types.bool;
            default = false;
            description = "Move navigation buttons and the address field into the vertical sidebar.";
          };

          discardHiddenTabsAfterMinutes = mkOption {
            type = types.ints.u32;
            default = 120;
            description = "Discard hidden tabs after this many minutes (0 disables discarding).";
          };

          tabProcesses = mkOption {
            type = types.enum constants.tabProcessesValues;
            default = "perOrigin";
            description = "WebContent process model ('perTab', 'perOrigin', or 'perSite').";
          };
        };
      };
      default = { };
      description = "Tab strip, sidebar and process isolation settings (`tabs`).";
    };

    browsingBehavior = mkOption {
      type = types.submodule {
        options = {
          enableAutoscroll = mkOption {
            type = types.bool;
            default = true;
            description = "Enable middle-click autoscrolling.";
          };

          enablePrimaryPaste = mkOption {
            type = types.bool;
            default = true;
            description = "Enable middle-click paste from the primary selection where supported.";
          };
        };
      };
      default = { };
      description = "Browsing interaction behaviour (`browsingBehavior`).";
    };

    searchEngine = mkOption {
      type = types.submodule {
        options = {
          engine = mkOption {
            type = types.nullOr types.str;
            default = null;
            example = "Kagi";
            description = ''
              Selected search engine name: one of the built-in engines
              (${lib.concatStringsSep ", " constants.builtinSearchEngines}) or a
              name defined in `searchEngine.custom`. Null disables omnibox search.
            '';
          };

          suggestions = mkOption {
            type = types.bool;
            default = false;
            description = "Enable search autocomplete suggestions from the selected engine.";
          };

          custom = mkOption {
            type = types.listOf customSearchEngineType;
            default = [ ];
            description = "Custom search engine definitions.";
          };
        };
      };
      default = { };
      description = "Omnibox search engine settings (`searchEngine`).";
    };

    autoplay = mkOption {
      type = types.submodule {
        options = {
          policy = mkOption {
            type = types.enum constants.autoplayPolicies;
            default = "block-audio";
            description = "Default multimedia autoplay policy.";
          };

          siteFilters = mkOption {
            type = types.attrsOf (types.enum constants.autoplayPolicies);
            default = { };
            example = {
              "https://www.youtube.com" = "allow-audio-and-video";
            };
            description = "Per-origin autoplay policy overrides.";
          };
        };
      };
      default = { };
      description = "Multimedia autoplay policy and site overrides (`autoplay`).";
    };

    permissions = mkOption {
      type = types.submodule {
        options = {
          manageSiteDecisions = mkOption {
            type = types.bool;
            default = false;
            description = ''
              Manage `Permissions.json` even when all per-site decision maps are
              empty. Automatically enabled whenever any `*.sites` entry is set.
            '';
          };

          geolocation = mkOption {
            type = permissionSectionType "geolocation";
            default = { };
            description = "Geolocation permission default policy and per-origin decisions.";
          };

          microphone = mkOption {
            type = permissionSectionType "microphone";
            default = { };
            description = "Microphone permission default policy and per-origin decisions.";
          };

          notifications = mkOption {
            type = permissionSectionType "notifications";
            default = { };
            description = "Notifications permission default policy and per-origin decisions.";
          };
        };
      };
      default = { };
      description = "Global permission policies (`Settings.json`) and per-site decisions (`Permissions.json`).";
    };

    globalPrivacyControl = mkOption {
      type = types.bool;
      default = false;
      description = "Send the Sec-GPC: 1 header and expose navigator.globalPrivacyControl.";
    };

    crashReportSending = mkOption {
      type = types.enum constants.crashReportSendingValues;
      default = "ask";
      description = "Crash report submission preference ('ask' or 'never').";
    };

    backgroundNetworking = mkOption {
      type = types.submodule {
        options = {
          enabled = mkOption {
            type = types.bool;
            default = true;
            description = "Master switch allowing background network access.";
          };

          contentBlockerSubscriptionUpdates = mkOption {
            type = types.bool;
            default = false;
            description = "Allow automatic background updates of content blocker subscriptions.";
          };

          updateChecks = mkOption {
            type = types.bool;
            default = true;
            description = "Allow automatic background browser update checks.";
          };
        };
      };
      default = { };
      description = "Background networking master toggle and per-feature flags (`backgroundNetworking`).";
    };

    contentBlockers = mkOption {
      type = types.submodule {
        options = {
          enabled = mkOption {
            type = types.bool;
            default = false;
            description = "Enable the built-in content blocker.";
          };

          disabledSites = mkOption {
            type = types.listOf types.str;
            default = [ ];
            example = [ "example.com" ];
            description = "Hostnames where content blocking is disabled (lowercase host without scheme or port).";
          };

          builtInLists = mkOption {
            type = builtInListsType;
            default = { };
            description = "Enable or disable each of the 27 built-in filter list subscriptions.";
          };

          customSubscriptions = mkOption {
            type = types.listOf customSubscriptionType;
            default = [ ];
            description = "Remote HTTP(S) filter list subscriptions.";
          };

          localLists = mkOption {
            type = types.listOf localContentBlockerListType;
            default = [ ];
            description = "Local filter lists registered in Settings.json (and optionally populated on disk).";
          };

          customFilters = mkOption {
            type = types.lines;
            default = "";
            description = "Inline custom content blocker filter rules (up to 4 MiB).";
          };
        };
      };
      default = { };
      description = "Content blocker configuration (`contentBlockers`).";
    };

    dns = mkOption {
      type = types.submodule {
        options = {
          mode = mkOption {
            type = types.enum constants.dnsModes;
            default = "system";
            description = "DNS resolution mode ('system' or 'custom').";
          };

          server = mkOption {
            type = types.nullOr types.str;
            default = null;
            example = "1.1.1.1";
            description = "Custom DNS server IP address or hostname (required when `mode = \"custom\"`).";
          };

          port = mkOption {
            type = types.nullOr types.ints.u16;
            default = null;
            description = "Custom DNS server port (defaults to 853 for 'tls' and 53 for 'udp').";
          };

          type = mkOption {
            type = types.enum constants.dnsTypes;
            default = "tls";
            description = "Transport for custom DNS ('tls' for DNS-over-TLS or 'udp').";
          };

          dnssec = mkOption {
            type = types.bool;
            default = false;
            description = "Validate DNSSEC records locally when using custom DNS.";
          };
        };
      };
      default = { };
      description = "DNS resolver configuration (`dnsSettings`).";
    };

    configVariables = mkOption {
      type = types.submodule {
        options =
          lib.mapAttrs (
            _: def:
            mkOption {
              type = configVarTypeFor def.type;
              inherit (def) default;
              description = "${def.description} (`${def.key}`)";
            }
          ) constants.configVariables
          // {
            extra = mkOption {
              type = types.attrsOf jsonFormat.type;
              default = { };
              description = "Additional raw `about:config` variables keyed by exact dotted name.";
            };
          };
      };
      default = { };
      description = "Typed `about:config` variables (`configVariables`).";
    };

    bookmarks = mkOption {
      type = types.nullOr (types.listOf bookmarkItemType);
      default = null;
      description = ''
        Declarative bookmark tree written to `Bookmarks.json` (schema v2). When
        null, `Bookmarks.json` is left untouched so interactively saved bookmarks
        are preserved.
      '';
    };

    extraSettings = mkOption {
      type = types.attrsOf jsonFormat.type;
      default = { };
      description = ''
        Freeform JSON attributes recursively merged into `Settings.json` after
        all typed options, acting as a forward-compatible escape hatch.
      '';
    };
  };

  profileSubmodule = types.submodule {
    options = profileOptions;
  };

  customPathProfileSubmodule = types.submodule {
    options = profileOptions // {
      path = mkOption {
        type = types.str;
        description = ''
          Absolute path to the self-contained profile directory passed via
          `--profile-path` (`<path>/config/Settings.json`, `<path>/data/...`).
        '';
      };
    };
  };

  jitOptionsType = types.submodule {
    options = {
      raw = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Raw `LIBJS_JIT` string. Overrides all structured JIT options when set.";
      };

      enable = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Explicitly pass `on` or `off` in `LIBJS_JIT`.";
      };

      threshold = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Invocations before a function tiers up (`threshold=N`).";
      };

      warmup = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Invocations before feedback collection starts (`warmup=N`).";
      };

      sync = mkOption {
        type = types.bool;
        default = false;
        description = "Compile hot functions synchronously on the main thread (`sync`).";
      };

      compileThreads = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Asynchronous JIT compiler thread count (`compile-threads=N`, 0 = automatic).";
      };

      calleeBoost = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Tier-up budget boost for interpreted callees called from JIT code (`callee-boost=N`).";
      };

      inlineBudget = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Total bytecode instruction budget for inlining candidates (`inline-budget=N`).";
      };

      inlineMaxSize = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Maximum bytecode size of a single inlining candidate (`inline-max-size=N`).";
      };

      inlineDepth = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Maximum call inlining nesting depth (`inline-depth=N`, 0 disables inlining).";
      };

      dumpIr = mkOption {
        type = types.bool;
        default = false;
        description = "Print JIT IR for each compilation (`dump-ir`).";
      };

      dumpPasses = mkOption {
        type = types.bool;
        default = false;
        description = "Print JIT IR after every optimisation pass (`dump-passes`).";
      };

      dumpAsm = mkOption {
        type = types.bool;
        default = false;
        description = "Print generated machine code for each compilation (`dump-asm`).";
      };

      dumpFeedback = mkOption {
        type = types.bool;
        default = false;
        description = "Print interpreter feedback when functions get hot (`dump-feedback`).";
      };

      verifyIr = mkOption {
        type = types.bool;
        default = false;
        description = "Verify IR invariants after every pass (`verify-ir`).";
      };

      logExits = mkOption {
        type = types.bool;
        default = false;
        description = "Log JIT exits and code discards to stderr (`log-exits`).";
      };

      perfMap = mkOption {
        type = types.bool;
        default = false;
        description = "Write `/tmp/perf-<pid>.map` symbols for profilers (`perf-map`).";
      };

      coverageDir = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Write per-VM JIT coverage JSON files to this directory (`coverage=DIR`).";
      };

      seed = mkOption {
        type = types.nullOr nonNegInt;
        default = null;
        description = "PRNG seed for JIT stress options (`seed=N`).";
      };

      stressExits = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Force random JIT exits every N checks (`stress-exits=N`).";
      };

      stressOsr = mkOption {
        type = types.bool;
        default = false;
        description = "Create OSR entries at every executed loop back edge (`stress-osr`).";
      };

      stressRegisters = mkOption {
        type = types.bool;
        default = false;
        description = "Restrict register allocation to call ABI registers (`stress-registers`).";
      };

      randomThresholds = mkOption {
        type = types.bool;
        default = false;
        description = "Randomise warmup and tier-up budgets (`random-thresholds`).";
      };

      stressInstall = mkOption {
        type = types.bool;
        default = false;
        description = "Delay asynchronous JIT installation randomly (`stress-install`).";
      };

      stressInvalidate = mkOption {
        type = types.nullOr types.ints.u32;
        default = null;
        description = "Force random dependency invalidations (`stress-invalidate=N`).";
      };

      extraOptions = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Additional comma-separated tokens appended to `LIBJS_JIT`.";
      };
    };
  };

  launchOptionsType = types.submodule {
    options = {
      userAgentPreset = mkOption {
        type = types.nullOr (types.enum constants.userAgentPresets);
        default = null;
        description = "User-Agent preset passed via `--user-agent-preset`.";
      };

      disableFeatures = mkOption {
        type = types.listOf (types.enum constants.platformFeatures);
        default = [ ];
        description = "Platform features disabled via `--disable-features`.";
      };

      forceEnableFeatures = mkOption {
        type = types.listOf (types.enum constants.platformFeatures);
        default = [ ];
        description = "Platform features forced on via `--force-enable-features`.";
      };

      devtoolsPort = mkOption {
        type = types.nullOr types.ints.u16;
        default = null;
        description = "Enable the Firefox DevTools server on this port (`--devtools=PORT`).";
      };

      webContentMemoryLimitMiB = mkOption {
        type = types.nullOr nonNegInt;
        default = null;
        description = "Per-WebContent memory cap in MiB (`--web-content-memory-limit`, 0 = unlimited).";
      };

      maxSiteRequestServers = mkOption {
        type = types.nullOr nonNegInt;
        default = null;
        description = "Maximum per-site RequestServer processes (`--max-site-request-servers`, 0 = shared).";
      };

      allowPopups = mkEnableOption "passing `--allow-popups`";
      disableScripting = mkEnableOption "passing `--disable-scripting`";
      disableSqlDatabase = mkEnableOption "passing `--disable-sql-database`";
      disableHttpMemoryCache = mkEnableOption "passing `--disable-http-memory-cache`";
      disableHttpDiskCache = mkEnableOption "passing `--disable-http-disk-cache`";
      disableContentBlocker = mkEnableOption "passing `--disable-content-blocker`";
      disableSandbox = mkEnableOption "passing `--disable-sandbox`";
      forceCpuPainting = mkEnableOption "passing `--force-cpu-painting`";
      forceFontconfig = mkEnableOption "passing `--force-fontconfig`";
      forceVideoDecoderServer = mkEnableOption "passing `--force-video-decoder-server`";
      enableAutoplay = mkEnableOption "passing `--enable-autoplay`";
      exposeExperimentalInterfaces = mkEnableOption "passing `--expose-experimental-interfaces`";
      logAllJsExceptions = mkEnableOption "passing `--log-all-js-exceptions`";
      disableScrollbarPainting = mkEnableOption "passing `--disable-scrollbar-painting`";
      tupleFileOrigins = mkEnableOption "passing `--tuple-file-origins`";

      defaultTimeZone = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "IANA time zone ID passed via `--default-time-zone`.";
      };

      certificates = mkOption {
        type = types.listOf (types.either types.path types.str);
        default = [ ];
        description = "Additional TLS certificate files passed via `--certificate`.";
      };

      contentBlockerLists = mkOption {
        type = types.listOf (types.either types.path types.str);
        default = [ ];
        description = "Extra content blocker list paths passed via `--content-blocker-list`.";
      };

      statsLogDir = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Directory for process statistics JSONL logs (`--stats-log-dir`).";
      };

      dnsOverride = mkOption {
        type = types.nullOr (
          types.submodule {
            options = {
              server = mkOption {
                type = types.str;
                description = "DNS server address (`--dns-server`).";
              };
              port = mkOption {
                type = types.nullOr types.ints.u16;
                default = null;
                description = "DNS server port (`--dns-port`).";
              };
              dot = mkOption {
                type = types.bool;
                default = false;
                description = "Use DNS-over-TLS (`--dot`).";
              };
              dnssec = mkOption {
                type = types.bool;
                default = false;
                description = "Validate DNSSEC locally (`--dnssec`).";
              };
            };
          }
        );
        default = null;
        description = "Command-line DNS override passed to the browser wrapper.";
      };

      jit = mkOption {
        type = jitOptionsType;
        default = { };
        description = "LibJS optimising JIT options exported via `LIBJS_JIT`.";
      };

      codeFlush = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "on";
        description = "Bytecode flushing options exported via `LIBJS_CODE_FLUSH` ('on', 'off', 'stress').";
      };

      gc = mkOption {
        type = types.submodule {
          options = {
            logLevel = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Value exported to `LIBGC_LOG_LEVEL`.";
            };
            moveEverything = mkEnableOption "setting `LIBGC_MOVE_EVERYTHING=1`";
            compact = mkEnableOption "setting `LIBGC_COMPACT=1`";
          };
        };
        default = { };
        description = "LibGC runtime environment variables.";
      };

      reportsEndpoint = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Custom crash/web-compat endpoint exported to `LADYBIRD_REPORTS_ENDPOINT`.";
      };

      sandboxDebug = mkEnableOption "setting `LADYBIRD_SANDBOX_DEBUG=1`";

      extraEnv = mkOption {
        type = types.attrsOf types.str;
        default = { };
        description = "Additional environment variables exported by the browser launcher.";
      };

      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Additional command-line arguments appended by the browser launcher.";
      };
    };
  };

  isValidProfileName = name: builtins.match "^[a-z0-9_-]+$" name != null;
  isValidBlockerId =
    id:
    builtins.stringLength id >= 1
    && builtins.stringLength id <= 128
    && builtins.match "^[A-Za-z0-9-]+$" id != null;
  isValidSearchUrl =
    url: (lib.hasPrefix "http://" url || lib.hasPrefix "https://" url) && lib.hasInfix "%s" url;
  isHttpUrl = url: lib.hasPrefix "http://" url || lib.hasPrefix "https://" url;

  checkBookmarks =
    prefix: items:
    lib.concatLists (
      lib.imap0 (
        idx: item:
        let
          itemType =
            if item.type != null then
              item.type
            else if item.url != null then
              "bookmark"
            else
              "folder";
          loc = "${prefix}[${toString idx}]";
        in
        [
          {
            assertion = itemType != "bookmark" || (item.url != null && item.url != "");
            message = "${loc}: bookmark items must set a non-empty `url`";
          }
          {
            assertion = itemType != "bookmark" || item.children == null;
            message = "${loc}: bookmark items cannot define `children`";
          }
          {
            assertion = itemType != "folder" || item.url == null;
            message = "${loc}: folder items cannot define `url`";
          }
        ]
        ++ lib.optionals (itemType == "folder" && item.children != null) (
          checkBookmarks "${loc}.children" item.children
        )
      ) items
    );

  profileAssertions =
    label: profile:
    let
      customNames = map (e: e.name) profile.searchEngine.custom;
      allEngineNames = constants.builtinSearchEngines ++ customNames;
      builtInIds = builtins.attrNames profile.contentBlockers.builtInLists;
      subIds = map (s: s.identifier) profile.contentBlockers.customSubscriptions;
      localIds = map (l: l.identifier) profile.contentBlockers.localLists;
      allBlockerIds = builtInIds ++ subIds ++ localIds;
    in
    [
      {
        assertion =
          profile.searchEngine.engine == null || builtins.elem profile.searchEngine.engine allEngineNames;
        message = "${label}.searchEngine.engine ('${toString profile.searchEngine.engine}') must be a built-in search engine or defined in searchEngine.custom";
      }
      {
        assertion = builtins.length customNames == builtins.length (lib.unique customNames);
        message = "${label}.searchEngine.custom contains duplicate search engine names";
      }
      {
        assertion = lib.all (name: !builtins.elem name constants.builtinSearchEngines) customNames;
        message = "${label}.searchEngine.custom cannot reuse a built-in search engine name";
      }
      {
        assertion = lib.all (e: e.name != "" && isValidSearchUrl e.url) profile.searchEngine.custom;
        message = "${label}.searchEngine.custom entries must have a non-empty name and an http(s):// URL containing '%s'";
      }
      {
        assertion = lib.all (
          e: e.suggestionsUrl == null || isValidSearchUrl e.suggestionsUrl
        ) profile.searchEngine.custom;
        message = "${label}.searchEngine.custom suggestionsUrl must be an http(s):// URL containing '%s'";
      }
      {
        assertion =
          profile.dns.mode != "custom" || (profile.dns.server != null && profile.dns.server != "");
        message = "${label}.dns.server must be set when dns.mode is 'custom'";
      }
      {
        assertion = lib.all (
          s: isValidBlockerId s.identifier && isHttpUrl s.url
        ) profile.contentBlockers.customSubscriptions;
        message = "${label}.contentBlockers.customSubscriptions entries require a valid identifier ([A-Za-z0-9-], 1..128) and http(s):// URL";
      }
      {
        assertion = lib.all (
          l: isValidBlockerId l.identifier && l.name != "" && !(l.text != null && l.source != null)
        ) profile.contentBlockers.localLists;
        message = "${label}.contentBlockers.localLists entries require a valid identifier, non-empty name, and at most one of text or source";
      }
      {
        assertion = builtins.length allBlockerIds == builtins.length (lib.unique allBlockerIds);
        message = "${label}.contentBlockers contains duplicate list identifiers across builtInLists, customSubscriptions, and localLists";
      }
    ]
    ++ lib.optionals (profile.bookmarks != null) (
      checkBookmarks "${label}.bookmarks" profile.bookmarks
    );

  selectorCount =
    (if cfg.defaultProfile != null then 1 else 0)
    + (if cfg.defaultProfilePath != null then 1 else 0)
    + (if cfg.temporaryProfile then 1 else 0);

  computedLaunchArgs = doggirlLib.mkLaunchArgs {
    inherit (cfg)
      defaultProfile
      defaultProfilePath
      temporaryProfile
      launch
      ;
  };

  computedLaunchEnv = doggirlLib.mkLaunchEnv cfg.launch;

  defaultWrappedPackage = doggirlLib.mkDoggirlWrapper pkgs {
    unwrapped = cfg.unwrappedPackage;
    inherit (cfg) localAppPath appName bundleIdentifier;
    extraArgs = computedLaunchArgs;
    extraEnv = computedLaunchEnv;
  };

  manifestEntries =
    lib.mapAttrsToList (
      name: profile:
      doggirlLib.mkProfileManifestEntry {
        inherit pkgs name profile;
      }
    ) cfg.profiles
    ++ lib.mapAttrsToList (
      name: profile:
      doggirlLib.mkProfileManifestEntry {
        inherit pkgs name profile;
        customPath = profile.path;
      }
    ) cfg.customPathProfiles;

  manifestFile = pkgs.writeText "doggirl-profiles-manifest.json" (builtins.toJSON manifestEntries);

  configApplyPkg = pkgs.callPackage ../packages/config-apply.nix { };
in
{
  options.programs.doggirl = {
    enable = mkEnableOption "doggirl (Ladybird hard fork) browser and declarative profile configuration";

    package = mkOption {
      type = types.nullOr types.package;
      default = defaultWrappedPackage;
      defaultText = lib.literalExpression "doggirl wrapper package generated from launch options";
      description = ''
        Browser package to install. Defaults to a wrapped launcher built from
        `unwrappedPackage` or `localAppPath` with `launch` flags and environment
        variables baked in. Set to `null` to manage profile configs only.
      '';
    };

    unwrappedPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = ''
        Optional Nix derivation providing a built `Ladybird.app` bundle or
        `bin/Ladybird` binary to wrap.
      '';
    };

    localAppPath = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/Volumes/dev/ladybird/Build/distribution/bin/Ladybird.app";
      description = ''
        Optional host path to a locally built `Ladybird.app` bundle or binary.
        Useful during active development so incremental CMake builds take effect
        immediately without rebuilding a Nix store package.
      '';
    };

    appName = mkOption {
      type = types.str;
      default = "Doggirl";
      description = "macOS `.app` bundle name (installed under `/Applications/Nix Apps/<appName>.app`).";
    };

    bundleIdentifier = mkOption {
      type = types.str;
      default = constants.bundleIdentifiers.development;
      description = "CFBundleIdentifier used for the macOS launcher bundle.";
    };

    defaultProfile = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "developer";
      description = ''
        Named profile passed via `--profile` by the launcher wrapper. Must match
        `[a-z0-9_-]+`. Note that development builds default to `developer` and
        release builds default to `default`.
      '';
    };

    defaultProfilePath = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Absolute self-contained profile directory passed via `--profile-path`.";
    };

    temporaryProfile = mkOption {
      type = types.bool;
      default = false;
      description = "Launch the browser with `--temporary-profile`.";
    };

    manageMode = mkOption {
      type = types.enum [
        "overwrite"
        "merge"
        "symlink"
      ];
      default = "overwrite";
      description = ''
        How declarative profile files are installed:
        - `overwrite`: atomically write generated JSON files (0600); runtime edits persist until next activation.
        - `merge`: deep-merge generated `Settings.json` over existing JSON via `jq`, preserving undeclared keys such as window geometry.
        - `symlink`: symlink directly to the read-only Nix store JSON files (strictly immutable).
      '';
    };

    backupExisting = mkOption {
      type = types.bool;
      default = true;
      description = "Save a `.bak` copy when overwriting a changed unmanaged file.";
    };

    configBaseDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Override base config root directory. Defaults to `~/Library/Preferences`
        on macOS and `~/.config` on Linux (matching `LibCore/StandardPaths.cpp`).
      '';
    };

    dataBaseDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Override base user data root directory. Defaults to
        `~/Library/Application Support` on macOS and `~/.local/share` on Linux.
      '';
    };

    launch = mkOption {
      type = launchOptionsType;
      default = { };
      description = "Command-line flags and runtime environment variables baked into the launcher wrapper.";
    };

    profiles = mkOption {
      type = types.attrsOf profileSubmodule;
      default = { };
      description = ''
        Named browser profiles written under
        `<configBaseDir>/Ladybird/Profiles/<name>/`. Profile names must match
        `[a-z0-9_-]+` (for example `developer` for local builds or `default` for
        release builds).
      '';
    };

    customPathProfiles = mkOption {
      type = types.attrsOf customPathProfileSubmodule;
      default = { };
      description = ''
        Self-contained profiles at explicit absolute paths (`--profile-path`),
        writing `<path>/config/{Settings,Permissions,Bookmarks}.json` and
        `<path>/data/ContentBlocking/Lists/<id>.txt`.
      '';
    };

    # Read-only computed helpers exposed for inspection and downstream modules/checks.
    generated = {
      manifestFile = mkOption {
        type = types.package;
        readOnly = true;
        internal = true;
        default = manifestFile;
        description = "Generated JSON manifest consumed by doggirl-config-apply.";
      };

      configApplyPackage = mkOption {
        type = types.package;
        readOnly = true;
        internal = true;
        default = configApplyPkg;
        description = "The doggirl-config-apply package.";
      };

      launchArgs = mkOption {
        type = types.listOf types.str;
        readOnly = true;
        internal = true;
        default = computedLaunchArgs;
        description = "Resolved CLI flags passed by the launcher wrapper.";
      };

      launchEnv = mkOption {
        type = types.attrsOf types.str;
        readOnly = true;
        internal = true;
        default = computedLaunchEnv;
        description = "Resolved environment variables exported by the launcher wrapper.";
      };

      hasProfiles = mkOption {
        type = types.bool;
        readOnly = true;
        internal = true;
        default = manifestEntries != [ ];
        description = "Whether any profiles or customPathProfiles are declared.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = selectorCount <= 1;
        message = "programs.doggirl: defaultProfile, defaultProfilePath, and temporaryProfile are mutually exclusive";
      }
      {
        assertion = cfg.defaultProfile == null || isValidProfileName cfg.defaultProfile;
        message = "programs.doggirl.defaultProfile ('${toString cfg.defaultProfile}') must match ^[a-z0-9_-]+$";
      }
      {
        assertion = cfg.defaultProfilePath == null || lib.hasPrefix "/" cfg.defaultProfilePath;
        message = "programs.doggirl.defaultProfilePath must be an absolute path";
      }
      {
        assertion = cfg.launch.dnsOverride == null || cfg.launch.dnsOverride.server != "";
        message = "programs.doggirl.launch.dnsOverride.server must be non-empty";
      }
    ]
    ++ lib.mapAttrsToList (name: _: {
      assertion = isValidProfileName name;
      message = "programs.doggirl.profiles.${name}: profile names may only contain lowercase ASCII letters, digits, '_' and '-'";
    }) cfg.profiles
    ++ lib.mapAttrsToList (name: profile: {
      assertion = lib.hasPrefix "/" profile.path;
      message = "programs.doggirl.customPathProfiles.${name}.path must be an absolute path";
    }) cfg.customPathProfiles
    ++ lib.concatLists (
      lib.mapAttrsToList (
        name: profile: profileAssertions "programs.doggirl.profiles.${name}" profile
      ) cfg.profiles
    )
    ++ lib.concatLists (
      lib.mapAttrsToList (
        name: profile: profileAssertions "programs.doggirl.customPathProfiles.${name}" profile
      ) cfg.customPathProfiles
    );
  };
}
