# Verified against Ladybird / doggirl C++ and Rust sources:
# - Libraries/LibWebView/Settings.h
# - Libraries/LibWebView/Settings.cpp
# - Libraries/LibWebView/SearchEngine.cpp
# - Libraries/LibWebView/UserAgent.cpp
# - Libraries/LibWebView/PermissionManager.cpp
# - Libraries/LibWebCommon/HTML/AutoplayPolicy.cpp
# - Libraries/LibWebCommon/Permissions/Permissions.cpp
# - Libraries/LibPlatformFeatures/Feature.cpp
# - Libraries/LibJS/Rust/src/jit_options.rs
# - UI/Qt/BookmarkStore.cpp
{
  builtinSearchEngines = [
    "Bing"
    "Brave"
    "DuckDuckGo"
    "Ecosia"
    "Google"
    "Kagi"
    "Mojeek"
    "Qwant"
    "Startpage"
    "Yahoo"
    "Yandex"
  ];

  # All 27 built-in content blocker lists registered in Settings::reset_to_defaults().
  builtinContentBlockerLists = [
    # General lists
    "easyList"
    "easyPrivacy"
    "easyListCookie"
    "fanboyAnnoyances"
    "fanboySocial"
    "adblockWarningRemoval"
    # Language-specific lists
    "abpindo"
    "abpvn"
    "bulgarianList"
    "nordicFilters"
    "easyListChina"
    "easyListCzechAndSlovak"
    "easyListDutch"
    "easyListGermany"
    "easyListHebrew"
    "easyListItaly"
    "easyListLithuania"
    "easyListPolish"
    "easyListPortuguese"
    "easyListSpanish"
    "indianList"
    "koreanList"
    "latvianList"
    "listeAR"
    "listeFR"
    "roList"
    "ruAdList"
  ];

  showBookmarksBarValues = [
    "always"
    "never"
    "onHover"
  ];

  themePreferences = [
    "system"
    "light"
    "dark"
  ];

  colorThemes = [
    "neutral"
    "harbor"
    "glacier"
    "lagoon"
    "moss"
    "saffron"
    "copper"
    "cherry"
    "blossom"
    "lavender"
  ];

  verticalTabsPositions = [
    "left"
    "right"
  ];

  tabProcessesValues = [
    "perTab"
    "perOrigin"
    "perSite"
  ];

  autoplayPolicies = [
    "allow-audio-and-video"
    "block-audio"
    "block-audio-and-video"
  ];

  permissionNames = [
    "geolocation"
    "microphone"
    "notifications"
  ];

  permissionPolicies = [
    "ask"
    "block"
  ];

  permissionSiteDecisions = [
    "allow"
    "block"
  ];

  crashReportSendingValues = [
    "ask"
    "never"
  ];

  dnsModes = [
    "system"
    "custom"
  ];

  dnsTypes = [
    "tls"
    "udp"
  ];

  userAgentPresets = [
    "Chrome Linux Desktop"
    "Chrome macOS Desktop"
    "Firefox Linux Desktop"
    "Firefox macOS Desktop"
    "Safari macOS Desktop"
    "Chrome Android Mobile"
    "Firefox Android Mobile"
    "Safari iOS Mobile"
  ];

  platformFeatures = [
    "gpu_painting"
    "ycbcr_import"
    "vulkan_video"
    "vulkan_video.bundled_ffmpeg"
    "vulkan_video.bundled_ffmpeg.direct_export"
    "vulkan_video.bundled_ffmpeg.vp9"
    "vulkan_video.bundled_ffmpeg.av1"
    "vulkan_video.bundled_ffmpeg.av1.keep_sequence_header_enable_ref_mvs_flag"
    "vulkan_video.system_ffmpeg"
    "vulkan_video.system_ffmpeg.direct_export"
    "vulkan_video.system_ffmpeg.h264"
    "vulkan_video.system_ffmpeg.h265"
  ];

  # Exact mapping from Nix option names under configVariables to Ladybird's
  # about:config keys and defaults in CONFIG_VARIABLE_DEFINITIONS.
  configVariables = {
    showWebContentProcessId = {
      key = "debug.process.show_web_content_process_id";
      default = false;
      type = "bool";
      description = "Append the active WebContent process ID to each tab title and tooltip.";
    };
    showAdvancedDebugMenu = {
      key = "debug.ui.show_advanced_debug_menu";
      default = false;
      type = "bool";
      description = "Show the Debug menu with tools for inspecting the browser.";
    };
    showTabPerformanceMonitor = {
      key = "debug.ui.show_tab_performance_monitor";
      default = false;
      type = "bool";
      description = "Show per-tab CPU, memory, network and frame rate telemetry in the toolbar.";
    };
    showFeedbackButton = {
      key = "ui.toolbar.show_feedback_button";
      default = true;
      type = "bool";
      description = "Show the feedback button at the end of the toolbar.";
    };
    useClientSideWindowDecorations = {
      key = "ui.window.use_client_side_decorations";
      default = true;
      type = "bool";
      description = "Use custom title bar and window controls instead of the system window frame.";
    };
    verticalTabsHoverExpandDelayMs = {
      key = "ui.tabs.vertical_tabs_hover_expand_delay_ms";
      default = 100;
      type = "nonNegInt";
      description = "Delay in milliseconds before collapsed vertical tabs begin expanding on hover.";
    };
    verticalTabsHoverExpandAnimationDurationMs = {
      key = "ui.tabs.vertical_tabs_hover_expand_animation_duration_ms";
      default = 100;
      type = "nonNegInt";
      description = "Duration in milliseconds of the vertical tab hover expansion animation.";
    };
    maximumDiskCacheSizeMb = {
      key = "network.http_cache.maximum_size_mb";
      default = 5120;
      type = "posInt";
      description = "Maximum size of the HTTP disk cache in MiB.";
    };
    maximumConnectionsPerDownload = {
      key = "downloads.maximum_connections_per_download";
      default = 4;
      type = "posInt";
      description = "Maximum parallel connections used for a single download when byte ranges are supported.";
    };
    splitDownloadsWithoutValidators = {
      key = "downloads.split_without_validators";
      default = false;
      type = "bool";
      description = "Allow multi-connection downloads even when ETag and Last-Modified headers are absent.";
    };
    restartStalledConnections = {
      key = "downloads.restart_stalled_connections";
      default = true;
      type = "bool";
      description = "Drop stalled connections in a multi-connection download and retry their byte range.";
    };
    disableHighDynamicRangeSurfaces = {
      key = "debug.compositor.disable_high_dynamic_range_surfaces";
      default = false;
      type = "bool";
      description = "Tone-map HDR content into SDR surfaces rather than presenting HDR surfaces.";
    };
    offerToTrustSelfSignedCertificates = {
      key = "security.certificates.offer_to_trust_self_signed";
      default = false;
      type = "bool";
      description = "Offer to temporarily trust self-signed TLS certificates for the current session.";
    };
    offerToTrustInvalidCertificates = {
      key = "security.certificates.offer_to_trust_invalid";
      default = false;
      type = "bool";
      description = "Offer to temporarily trust other TLS certificates that fail verification for the current session.";
    };
    updateCheckIntervalMinutes = {
      key = "updates.check_interval_minutes";
      default = 240;
      type = "min5Int";
      description = "Minutes between automatic release update checks (at least 5).";
    };
  };

  bundleIdentifiers = {
    release = "dev.dappermint.doggirl";
    development = "dev.dappermint.doggirl.Development";
  };
}
