use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Zh,
    En,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// npm dist-tag used when resolving Harness updates. `Latest` preserves the
/// existing conservative behavior, while `Alpha` opts into official preview
/// builds explicitly published on the alpha channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum HarnessUpdateChannel {
    #[default]
    Latest,
    Alpha,
}

impl HarnessUpdateChannel {
    pub fn dist_tag(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Alpha => "alpha",
        }
    }
}

/// Proxy mode for every Launcher-owned network operation. `System` is the
/// default so installations and configurations written before proxy support
/// keep their previous behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    #[default]
    System,
    Direct,
    Manual,
}

/// Proxy configuration. Every persistence and snapshot boundary canonicalizes
/// this value through `network::for_persistence`: manual URLs carrying
/// userinfo are rejected, while inactive URL/bypass fields are erased.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    pub mode: ProxyMode,
    /// Single proxy URL for manual mode (http, https, socks5, socks5h).
    #[serde(default)]
    pub url: String,
    /// Optional comma/semicolon separated bypass (NO_PROXY) list for manual
    /// mode.
    #[serde(default)]
    pub bypass: String,
}

/// Conservative classification of a failed network operation. Anything that
/// does not clearly match a known category stays `Other` so diagnostics never
/// claim a cause they cannot prove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum NetworkErrorKind {
    Timeout,
    ProxyAuth,
    Tls,
    Connect,
    HttpStatus,
    #[default]
    Other,
}

impl NetworkErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ProxyAuth => "proxyAuth",
            Self::Tls => "tls",
            Self::Connect => "connect",
            Self::HttpStatus => "httpStatus",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "timeout" => Self::Timeout,
            "proxyAuth" => Self::ProxyAuth,
            "tls" => Self::Tls,
            "connect" => Self::Connect,
            "httpStatus" => Self::HttpStatus,
            _ => Self::Other,
        }
    }
}

/// A Harness registry that answered the proxy connection test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestSource {
    pub source: String,
    pub version: String,
}

/// A Harness registry that failed the proxy connection test. `detail` is
/// sanitized: proxy credentials and URL userinfo never appear in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestFailure {
    pub source: String,
    pub kind: NetworkErrorKind,
    pub detail: String,
}

/// Outcome of a proxy connection test across all Harness registries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestReport {
    pub sources: Vec<ProxyTestSource>,
    pub failures: Vec<ProxyTestFailure>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum LauncherStep {
    #[default]
    Prepare,
    Start,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum LauncherPhase {
    #[default]
    Preparing,
    AwaitingMigration,
    Starting,
    Ready,
    Stopped,
    Failed,
    Stopping,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPlan {
    pub source_entries: u32,
    pub workspace_available: bool,
    pub cc_switch_providers: u32,
}

impl MigrationPlan {
    pub fn has_data(&self) -> bool {
        self.source_entries > 0 || self.workspace_available || self.cc_switch_providers > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MigrationState {
    #[default]
    NotRequired,
    Pending {
        plan: MigrationPlan,
    },
    Applying {
        plan: MigrationPlan,
    },
    Completed,
    CompletedWithWarning {
        warning: AppError,
    },
    Skipped,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BrowserChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ActivityCode {
    WaitingForLock,
    CheckingRuntime,
    ResolvingVersion,
    DownloadingNode,
    VerifyingNode,
    CheckingSources,
    CopyingHarnessRuntime,
    InstallingHarness,
    ResolvingHarnessDependencies,
    DownloadingHarnessPackages,
    WritingHarnessRuntime,
    ValidatingHarness,
    ActivatingHarness,
    MigratingData,
    RepairingStartup,
    StartingService,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ActivityState {
    pub code: ActivityCode,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[ts(type = "number")]
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ProgressState {
    #[default]
    Indeterminate,
    Determinate {
        #[ts(type = "number")]
        done: u64,
        #[ts(type = "number")]
        total: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DesktopUpdateState {
    #[default]
    Idle,
    Checking,
    Available {
        version: String,
    },
    Preparing {
        version: String,
    },
    Downloading {
        version: String,
        #[ts(type = "number")]
        done: u64,
        #[ts(type = "number | null")]
        total: Option<u64>,
    },
    Installing {
        version: String,
    },
    Failed {
        version: Option<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HarnessUpdateState {
    #[default]
    None,
    Checking,
    Available {
        version: String,
    },
    Downloading {
        version: String,
    },
    Downloaded {
        version: String,
    },
    Installing {
        version: String,
    },
    Failed {
        version: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum HarnessUpdateMode {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum RemoteScope {
    #[default]
    Lan,
    Public,
}

/// Lifecycle of the public tunnel, independent from the LAN listener.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum RemoteTunnelState {
    #[default]
    Off,
    /// cloudflared is being downloaded or the tunnel is connecting.
    Starting,
    Running,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RemoteLanSnapshot {
    pub enabled: bool,
    /// True when the host currently has a non-loopback IPv4 route. Ethernet
    /// and Wi-Fi are both valid LAN transports.
    pub available: bool,
    /// Listening and the upstream Harness web UI is reachable. The QR target.
    pub url: Option<String>,
    pub password: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RemotePublicSnapshot {
    pub enabled: bool,
    pub state: RemoteTunnelState,
    /// Assigned trycloudflare URL once the tunnel is up. The QR target.
    pub url: Option<String>,
    pub password: String,
    pub error: Option<AppError>,
}

/// Owner-facing remote-access state. Passwords are included deliberately:
/// only the desktop operator sees this snapshot, and the UI needs them to
/// render the connection cards. They never leave the local IPC boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSnapshot {
    pub master: bool,
    /// True when the Harness web UI is running and a proxy upstream exists.
    pub service_ready: bool,
    pub lan: RemoteLanSnapshot,
    pub public: RemotePublicSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LauncherSnapshot {
    #[ts(type = "number")]
    pub revision: u64,
    pub market_busy: bool,
    #[ts(type = "number")]
    pub market_revision: u64,
    #[ts(type = "number")]
    pub market_catalog_revision: u64,
    pub phase: LauncherPhase,
    pub step: LauncherStep,
    pub activity: Option<ActivityState>,
    pub progress: ProgressState,
    pub error: Option<AppError>,
    pub web_url: Option<String>,
    #[ts(type = "number | null")]
    pub service_started_at_ms: Option<u64>,
    pub browsers: Vec<BrowserChoice>,
    pub selected_browser_id: String,
    pub language: Language,
    pub theme: ThemePreference,
    pub desktop_version: String,
    pub harness_version: Option<String>,
    /// Last validated runtime retained beside the active Harness. Exposed so
    /// a failed service start can offer an explicit, version-bound rollback.
    pub previous_harness_version: Option<String>,
    /// Third-party profile packages removed transactionally after they were
    /// identified in startup output as incompatible. The UI acknowledges the
    /// recovery in a modal; an empty list means no recovery was committed.
    pub removed_incompatible_plugins: Vec<String>,
    /// True after the launcher isolated version-incompatible projection cache
    /// records and verified that Harness rebuilt them from authoritative logs.
    pub repaired_projection_cache: bool,
    pub desktop_update: DesktopUpdateState,
    pub harness_update: HarnessUpdateState,
    pub migration: MigrationState,
    pub tray_available: bool,
    pub show_balance_card: bool,
    pub harness_update_channel: HarnessUpdateChannel,
    pub proxy: ProxySettings,
    pub remote: RemoteSnapshot,
    pub pet: crate::pet::PetPreferences,
}

impl LauncherSnapshot {
    pub fn initial(desktop_version: impl Into<String>) -> Self {
        Self {
            revision: 0,
            market_busy: false,
            market_revision: 0,
            market_catalog_revision: 0,
            phase: LauncherPhase::Preparing,
            step: LauncherStep::Prepare,
            activity: None,
            progress: ProgressState::Indeterminate,
            error: None,
            web_url: None,
            service_started_at_ms: None,
            browsers: Vec::new(),
            selected_browser_id: "system".into(),
            language: Language::default(),
            theme: ThemePreference::default(),
            desktop_version: desktop_version.into(),
            harness_version: None,
            previous_harness_version: None,
            removed_incompatible_plugins: Vec::new(),
            repaired_projection_cache: false,
            desktop_update: DesktopUpdateState::default(),
            harness_update: HarnessUpdateState::default(),
            migration: MigrationState::default(),
            tray_available: false,
            show_balance_card: true,
            harness_update_channel: HarnessUpdateChannel::default(),
            proxy: ProxySettings::default(),
            remote: RemoteSnapshot::default(),
            pet: crate::pet::PetPreferences::default(),
        }
    }
}
