use std::{env, fs, path::PathBuf};

use dsh_core::{
    ActivityCode, ActivityState, AppError, BrowserChoice, DesktopUpdateState, HarnessUpdateMode,
    HarnessUpdateState, Language, LauncherPhase, LauncherSnapshot, LauncherStep, MigrationPlan,
    MigrationState, ProgressState, ThemePreference,
    balance::{BalanceSnapshot, BalanceStatus},
    marketplace::{
        CompatibilityInfo, CompatibilityStatus, InstalledPlugin, MarketCatalogState,
        MarketOperationKind, MarketOperationResult, MarketPage, MarketQuery, MarketSort,
        PendingMarketChange, PendingVerification, PluginKind, PluginSource, PluginSummary,
        SourceBindingStatus,
    },
};
use ts_rs::TS;

fn main() {
    let destination = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("bindings output path");
    let declarations = [
        Language::decl(),
        ThemePreference::decl(),
        LauncherPhase::decl(),
        LauncherStep::decl(),
        BrowserChoice::decl(),
        ActivityCode::decl(),
        ActivityState::decl(),
        ProgressState::decl(),
        AppError::decl(),
        DesktopUpdateState::decl(),
        HarnessUpdateState::decl(),
        HarnessUpdateMode::decl(),
        MigrationPlan::decl(),
        MigrationState::decl(),
        LauncherSnapshot::decl(),
        PluginKind::decl(),
        PluginSource::decl(),
        CompatibilityStatus::decl(),
        SourceBindingStatus::decl(),
        CompatibilityInfo::decl(),
        InstalledPlugin::decl(),
        PluginSummary::decl(),
        MarketSort::decl(),
        MarketQuery::decl(),
        MarketPage::decl(),
        MarketCatalogState::decl(),
        MarketOperationKind::decl(),
        MarketOperationResult::decl(),
        PendingMarketChange::decl(),
        PendingVerification::decl(),
        BalanceStatus::decl(),
        BalanceSnapshot::decl(),
    ];
    let mut output =
        String::from("// Generated from dsh-core Rust types. Do not edit by hand.\n\n");
    for declaration in declarations {
        output.push_str("export ");
        output.push_str(&declaration);
        output.push_str("\n\n");
    }
    fs::write(destination, output).expect("write bindings");
}
