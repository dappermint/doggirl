{ lib, constants }:

let
  # Format a 32-hex-char prefix of a SHA-256 digest into a deterministic UUID-like ID.
  deterministicUuid =
    seed:
    let
      h = builtins.hashString "sha256" seed;
    in
    "${builtins.substring 0 8 h}-${builtins.substring 8 4 h}-${builtins.substring 12 4 h}-${builtins.substring 16 4 h}-${builtins.substring 20 12 h}";

  serializeBookmarkItem =
    pathPrefix: index: item:
    let
      itemType =
        if item.type != null then
          item.type
        else if item.url != null then
          "bookmark"
        else
          "folder";
      seed = "${pathPrefix}/${toString index}/${itemType}/${
        if item.title != null then item.title else ""
      }/${if item.url != null then item.url else ""}";
      id = if item.id != null then item.id else deterministicUuid seed;
      base = {
        inherit id;
        type = itemType;
        inherit (item) dateAdded lastModified;
      }
      // lib.optionalAttrs (item.title != null) { inherit (item) title; };
    in
    if itemType == "bookmark" then
      base
      // {
        inherit (item) url;
      }
      // lib.optionalAttrs (item.faviconHash != null) {
        inherit (item) faviconHash;
      }
    else
      base
      // {
        children = lib.imap0 (i: child: serializeBookmarkItem "${pathPrefix}/${id}" i child) (
          if item.children != null then item.children else [ ]
        );
      };

  mkSettingsAttrset =
    profile:
    let
      windowAttrs =
        if profile.window == null then
          { }
        else
          {
            window = {
              size = {
                inherit (profile.window.size) width height;
              };
              inherit (profile.window) maximized;
            }
            // lib.optionalAttrs (profile.window.position != null) {
              position = {
                inherit (profile.window.position) x y;
              };
            };
          };

      tabAttrs = {
        inherit (profile.tabs)
          verticalTabsEnabled
          verticalTabsExpanded
          verticalTabsExpandOnHover
          verticalTabsPosition
          everythingInSidebar
          discardHiddenTabsAfterMinutes
          tabProcesses
          ;
      }
      // lib.optionalAttrs (profile.tabs.verticalTabsExpandedWidth != null) {
        inherit (profile.tabs) verticalTabsExpandedWidth;
      };

      customSearchEngines = map (
        engine:
        {
          inherit (engine) name url;
        }
        // lib.optionalAttrs (engine.suggestionsUrl != null) {
          inherit (engine) suggestionsUrl;
        }
      ) profile.searchEngine.custom;

      searchEngineAttrs = {
        inherit (profile.searchEngine) engine suggestions;
      }
      // lib.optionalAttrs (customSearchEngines != [ ]) {
        custom = customSearchEngines;
      };

      autoplaySiteFilters = lib.mapAttrsToList (filter: policy: {
        inherit filter policy;
      }) profile.autoplay.siteFilters;

      builtInLists = lib.mapAttrs (_: enabled: enabled) profile.contentBlockers.builtInLists;

      customSubscriptions = map (sub: {
        inherit (sub) identifier url enabled;
      }) profile.contentBlockers.customSubscriptions;

      localLists = map (list: {
        inherit (list) identifier name enabled;
      }) profile.contentBlockers.localLists;

      dnsSettings =
        if profile.dns.mode == "system" then
          { mode = "system"; }
        else
          {
            mode = "custom";
            inherit (profile.dns) server type dnssec;
            port =
              if profile.dns.port != null then
                profile.dns.port
              else if profile.dns.type == "tls" then
                853
              else
                53;
          };

      typedConfigVars = lib.listToAttrs (
        lib.mapAttrsToList (optName: def: {
          name = def.key;
          value = profile.configVariables.${optName};
        }) constants.configVariables
      );

      baseSettings = {
        inherit (profile)
          firstRunComplete
          newTabPageURL
          restorePreviousSession
          translationPrefersHigherQuality
          globalPrivacyControl
          crashReportSending
          ;

        appearance = {
          inherit (profile.appearance)
            showAllTabsButton
            showSidebarButton
            showMenuBar
            alwaysShowScrollbars
            showBookmarksBar
            interfaceTheme
            websiteColorScheme
            colorTheme
            ;
        };

        content = {
          inherit (profile.content)
            defaultZoomLevelFactor
            zoomPerHost
            enableForceDark
            forceDarkPerHost
            ;
        };

        tabs = tabAttrs;

        browsingBehavior = {
          inherit (profile.browsingBehavior)
            enableAutoscroll
            enablePrimaryPaste
            ;
        };

        searchEngine = searchEngineAttrs;

        autoplay = {
          inherit (profile.autoplay) policy;
          siteFilters = autoplaySiteFilters;
        };

        geolocation = {
          inherit (profile.permissions.geolocation) policy;
        };
        microphone = {
          inherit (profile.permissions.microphone) policy;
        };
        notifications = {
          inherit (profile.permissions.notifications) policy;
        };

        backgroundNetworking = {
          inherit (profile.backgroundNetworking) enabled;
          features = {
            inherit (profile.backgroundNetworking)
              contentBlockerSubscriptionUpdates
              updateChecks
              ;
          };
        };

        contentBlockers = {
          inherit (profile.contentBlockers)
            enabled
            disabledSites
            customFilters
            ;
          inherit
            builtInLists
            customSubscriptions
            localLists
            ;
        };

        inherit dnsSettings;

        configVariables = typedConfigVars // profile.configVariables.extra;
      }
      // windowAttrs
      // lib.optionalAttrs (profile.languages != null) {
        inherit (profile) languages;
      };
    in
    lib.recursiveUpdate baseSettings profile.extraSettings;

  mkPermissionsAttrset =
    profile:
    let
      decisionsFor =
        permissionName:
        lib.mapAttrsToList (origin: decision: {
          inherit origin decision;
          permission = permissionName;
        }) profile.permissions.${permissionName}.sites;
      decisions = lib.concatMap decisionsFor constants.permissionNames;
    in
    if profile.permissions.manageSiteDecisions || decisions != [ ] then
      {
        version = 1;
        inherit decisions;
        embargoes = [ ];
      }
    else
      null;

  mkBookmarksAttrset =
    profile:
    if profile.bookmarks == null then
      null
    else
      {
        version = 2;
        items = lib.imap0 (i: item: serializeBookmarkItem "root" i item) profile.bookmarks;
      };

  mkLibjsJitEnv =
    jit:
    if jit.raw != null then
      jit.raw
    else
      let
        tokens =
          lib.optional (jit.enable == false) "off"
          ++ lib.optional (jit.enable == true) "on"
          ++ lib.optional (jit.threshold != null) "threshold=${toString jit.threshold}"
          ++ lib.optional (jit.warmup != null) "warmup=${toString jit.warmup}"
          ++ lib.optional jit.sync "sync"
          ++ lib.optional (jit.compileThreads != null) "compile-threads=${toString jit.compileThreads}"
          ++ lib.optional (jit.calleeBoost != null) "callee-boost=${toString jit.calleeBoost}"
          ++ lib.optional (jit.inlineBudget != null) "inline-budget=${toString jit.inlineBudget}"
          ++ lib.optional (jit.inlineMaxSize != null) "inline-max-size=${toString jit.inlineMaxSize}"
          ++ lib.optional (jit.inlineDepth != null) "inline-depth=${toString jit.inlineDepth}"
          ++ lib.optional jit.dumpIr "dump-ir"
          ++ lib.optional jit.dumpPasses "dump-passes"
          ++ lib.optional jit.dumpAsm "dump-asm"
          ++ lib.optional jit.dumpFeedback "dump-feedback"
          ++ lib.optional jit.verifyIr "verify-ir"
          ++ lib.optional jit.logExits "log-exits"
          ++ lib.optional jit.perfMap "perf-map"
          ++ lib.optional (jit.coverageDir != null) "coverage=${jit.coverageDir}"
          ++ lib.optional (jit.seed != null) "seed=${toString jit.seed}"
          ++ lib.optional (jit.stressExits != null) "stress-exits=${toString jit.stressExits}"
          ++ lib.optional jit.stressOsr "stress-osr"
          ++ lib.optional jit.stressRegisters "stress-registers"
          ++ lib.optional jit.randomThresholds "random-thresholds"
          ++ lib.optional jit.stressInstall "stress-install"
          ++ lib.optional (jit.stressInvalidate != null) "stress-invalidate=${toString jit.stressInvalidate}"
          ++ jit.extraOptions;
      in
      if tokens == [ ] then null else lib.concatStringsSep "," tokens;

  mkLaunchArgs =
    {
      defaultProfile ? null,
      defaultProfilePath ? null,
      temporaryProfile ? false,
      launch,
    }:
    lib.optionals (defaultProfile != null) [
      "--profile"
      defaultProfile
    ]
    ++ lib.optionals (defaultProfilePath != null) [
      "--profile-path"
      defaultProfilePath
    ]
    ++ lib.optional temporaryProfile "--temporary-profile"
    ++ lib.optionals (launch.userAgentPreset != null) [
      "--user-agent-preset"
      launch.userAgentPreset
    ]
    ++ lib.optionals (launch.disableFeatures != [ ]) [
      "--disable-features"
      (lib.concatStringsSep "," launch.disableFeatures)
    ]
    ++ lib.optionals (launch.forceEnableFeatures != [ ]) [
      "--force-enable-features"
      (lib.concatStringsSep "," launch.forceEnableFeatures)
    ]
    ++ lib.optional (launch.devtoolsPort != null) "--devtools=${toString launch.devtoolsPort}"
    ++ lib.optionals (launch.webContentMemoryLimitMiB != null) [
      "--web-content-memory-limit"
      (toString launch.webContentMemoryLimitMiB)
    ]
    ++ lib.optionals (launch.maxSiteRequestServers != null) [
      "--max-site-request-servers"
      (toString launch.maxSiteRequestServers)
    ]
    ++ lib.optional launch.allowPopups "--allow-popups"
    ++ lib.optional launch.disableScripting "--disable-scripting"
    ++ lib.optional launch.disableSqlDatabase "--disable-sql-database"
    ++ lib.optional launch.disableHttpMemoryCache "--disable-http-memory-cache"
    ++ lib.optional launch.disableHttpDiskCache "--disable-http-disk-cache"
    ++ lib.optional launch.disableContentBlocker "--disable-content-blocker"
    ++ lib.optional launch.disableSandbox "--disable-sandbox"
    ++ lib.optional launch.forceCpuPainting "--force-cpu-painting"
    ++ lib.optional launch.forceFontconfig "--force-fontconfig"
    ++ lib.optional launch.forceVideoDecoderServer "--force-video-decoder-server"
    ++ lib.optional launch.enableAutoplay "--enable-autoplay"
    ++ lib.optional launch.exposeExperimentalInterfaces "--expose-experimental-interfaces"
    ++ lib.optional launch.logAllJsExceptions "--log-all-js-exceptions"
    ++ lib.optional launch.disableScrollbarPainting "--disable-scrollbar-painting"
    ++ lib.optional launch.tupleFileOrigins "--tuple-file-origins"
    ++ lib.optionals (launch.defaultTimeZone != null) [
      "--default-time-zone"
      launch.defaultTimeZone
    ]
    ++ lib.concatMap (cert: [
      "--certificate"
      (toString cert)
    ]) launch.certificates
    ++ lib.concatMap (listPath: [
      "--content-blocker-list"
      (toString listPath)
    ]) launch.contentBlockerLists
    ++ lib.optionals (launch.statsLogDir != null) [
      "--stats-log-dir"
      launch.statsLogDir
    ]
    ++ lib.optionals (launch.dnsOverride != null) (
      [
        "--dns-server"
        launch.dnsOverride.server
      ]
      ++ lib.optionals (launch.dnsOverride.port != null) [
        "--dns-port"
        (toString launch.dnsOverride.port)
      ]
      ++ lib.optional launch.dnsOverride.dot "--dot"
      ++ lib.optional launch.dnsOverride.dnssec "--dnssec"
    )
    ++ launch.extraArgs;

  mkLaunchEnv =
    launch:
    let
      jitValue = mkLibjsJitEnv launch.jit;
    in
    lib.optionalAttrs (jitValue != null) { LIBJS_JIT = jitValue; }
    // lib.optionalAttrs (launch.codeFlush != null) { LIBJS_CODE_FLUSH = launch.codeFlush; }
    // lib.optionalAttrs (launch.gc.logLevel != null) { LIBGC_LOG_LEVEL = launch.gc.logLevel; }
    // lib.optionalAttrs launch.gc.moveEverything { LIBGC_MOVE_EVERYTHING = "1"; }
    // lib.optionalAttrs launch.gc.compact { LIBGC_COMPACT = "1"; }
    // lib.optionalAttrs (launch.reportsEndpoint != null) {
      LADYBIRD_REPORTS_ENDPOINT = launch.reportsEndpoint;
    }
    // lib.optionalAttrs launch.sandboxDebug { LADYBIRD_SANDBOX_DEBUG = "1"; }
    // launch.extraEnv;

  mkProfileManifestEntry =
    {
      pkgs,
      name,
      profile,
      customPath ? null,
    }:
    let
      settingsFile = pkgs.writeText "doggirl-${name}-Settings.json" (
        builtins.toJSON (mkSettingsAttrset profile)
      );
      permissionsAttrset = mkPermissionsAttrset profile;
      permissionsFile =
        if permissionsAttrset == null then
          null
        else
          pkgs.writeText "doggirl-${name}-Permissions.json" (builtins.toJSON permissionsAttrset);
      bookmarksAttrset = mkBookmarksAttrset profile;
      bookmarksFile =
        if bookmarksAttrset == null then
          null
        else
          pkgs.writeText "doggirl-${name}-Bookmarks.json" (builtins.toJSON bookmarksAttrset);
      localListFiles = lib.filter (entry: entry != null) (
        map (
          list:
          if list.source != null then
            {
              inherit (list) identifier;
              source = toString list.source;
            }
          else if list.text != null then
            {
              inherit (list) identifier;
              source = toString (pkgs.writeText "doggirl-${name}-filter-${list.identifier}.txt" list.text);
            }
          else
            null
        ) profile.contentBlockers.localLists
      );
    in
    {
      inherit
        name
        customPath
        settingsFile
        permissionsFile
        bookmarksFile
        localListFiles
        ;
    };
in
{
  inherit
    deterministicUuid
    mkSettingsAttrset
    mkPermissionsAttrset
    mkBookmarksAttrset
    mkLibjsJitEnv
    mkLaunchArgs
    mkLaunchEnv
    mkProfileManifestEntry
    ;
}
