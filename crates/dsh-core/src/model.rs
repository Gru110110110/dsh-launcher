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
    pub desktop_update: DesktopUpdateState,
    pub harness_update: HarnessUpdateState,
    pub migration: MigrationState,
    pub tray_available: bool,
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
            desktop_update: DesktopUpdateState::default(),
            harness_update: HarnessUpdateState::default(),
            migration: MigrationState::default(),
            tray_available: false,
        }
    }
}
