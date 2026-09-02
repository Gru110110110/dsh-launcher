use std::sync::Arc;

use dsh_core::{
    AppError, HarnessUpdateChannel, HarnessUpdateMode, Language, LauncherSnapshot, ProxySettings,
    ProxyTestReport, StartupRepairBackupSummary, ThemePreference,
    balance::BalanceSnapshot,
    marketplace::{
        CompatibilityInfo, InstalledPlugin, MarketCatalogState, MarketOperationResult, MarketPage,
        MarketQuery, PendingVerification, PluginSummary,
    },
    pet::{PetPreferencesPatch, PetSnapshot},
};
use tauri::{AppHandle, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::application::AppState;

#[tauri::command]
pub fn launcher_get_snapshot(state: State<'_, Arc<AppState>>) -> LauncherSnapshot {
    state.snapshot()
}

#[tauri::command]
pub fn launcher_retry(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.inner().start(false, None)
}

#[tauri::command]
pub async fn launcher_rollback_harness(
    expected_version: String,
    state: State<'_, Arc<AppState>>,
) -> Result<String, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.rollback_harness(expected_version))
            .await,
    )
}

#[tauri::command]
pub async fn launcher_repair_and_start(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.repair_and_start()).await,
    )
}

#[tauri::command]
pub fn launcher_acknowledge_startup_repair(state: State<'_, Arc<AppState>>) {
    state.acknowledge_startup_repair();
}

#[tauri::command]
pub async fn launcher_startup_repair_backups(
    state: State<'_, Arc<AppState>>,
) -> Result<StartupRepairBackupSummary, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.startup_repair_backup_summary()).await,
    )
}

#[tauri::command]
pub async fn launcher_clear_startup_repair_backups(
    state: State<'_, Arc<AppState>>,
) -> Result<StartupRepairBackupSummary, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.clear_startup_repair_backups()).await,
    )
}

#[tauri::command]
pub async fn launcher_stop(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.stop_service()).await,
    )
}

#[tauri::command]
pub async fn launcher_restart(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.restart_service()).await,
    )
}

#[tauri::command]
pub fn launcher_update_harness(
    mode: HarnessUpdateMode,
    expected_version: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.inner().update_harness(mode, expected_version)
}

#[tauri::command]
pub fn launcher_activate_harness_update(
    expected_version: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.inner().activate_harness_update(expected_version)
}

#[tauri::command]
pub async fn launcher_check_harness_update(
    state: State<'_, Arc<AppState>>,
) -> Result<Option<String>, AppError> {
    state.inner().check_harness_update().await
}

#[tauri::command]
pub fn migration_approve(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.inner().approve_migration()
}

#[tauri::command]
pub fn migration_skip(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.inner().skip_migration()
}

#[tauri::command]
pub fn launcher_select_browser(
    browser_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.select_browser(browser_id)
}

#[tauri::command]
pub fn launcher_open_web_ui(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.open_web_ui()
}

#[tauri::command]
pub fn application_open_website(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.open_website()
}

#[tauri::command]
pub fn application_open_external_link(
    target: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.open_external_link(&target)
}

#[tauri::command]
pub fn application_copy_web_url(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    app.clipboard()
        .write_text(state.web_url()?)
        .map_err(|error| AppError::new("clipboardFailed").detail(error.to_string()))
}

#[tauri::command]
pub fn preferences_set_language(
    language: Language,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.set_language(language)
}

#[tauri::command]
pub fn preferences_set_theme(
    theme: ThemePreference,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.set_theme(theme)
}

#[tauri::command]
pub fn preferences_set_show_balance_card(
    show: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.set_show_balance_card(show)
}

#[tauri::command]
pub fn preferences_patch_pet(
    patch: PetPreferencesPatch,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.patch_pet_preferences(patch)
}

#[tauri::command]
pub fn pet_get_snapshot(state: State<'_, Arc<AppState>>) -> PetSnapshot {
    state.pet_snapshot()
}

#[tauri::command]
pub fn preferences_set_harness_update_channel(
    channel: HarnessUpdateChannel,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.inner().set_harness_update_channel(channel)
}

/// Validates, atomically saves, and immediately activates proxy settings.
#[tauri::command]
pub fn preferences_set_proxy(
    proxy: ProxySettings,
    state: State<'_, Arc<AppState>>,
) -> Result<bool, AppError> {
    state.set_proxy(proxy)
}

/// Tests the candidate proxy configuration from the settings form against the
/// Harness registries. The candidate is never persisted by this command.
#[tauri::command]
pub async fn proxy_test_connection(
    proxy: ProxySettings,
    state: State<'_, Arc<AppState>>,
) -> Result<ProxyTestReport, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.test_proxy(proxy)).await,
    )
}

/// Sanitized official balance state; credential values never leave Harness.
#[tauri::command]
pub async fn balance_get_snapshot(
    state: State<'_, Arc<AppState>>,
) -> Result<BalanceSnapshot, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || Ok(state.balance_snapshot(false))).await,
    )
}

/// Force a balance re-fetch, bypassing the bridge's five-minute cache.
#[tauri::command]
pub async fn balance_refresh(state: State<'_, Arc<AppState>>) -> Result<BalanceSnapshot, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || Ok(state.balance_snapshot(true))).await,
    )
}

#[tauri::command]
pub async fn application_check_update(
    state: State<'_, Arc<AppState>>,
) -> Result<Option<String>, AppError> {
    state.inner().check_desktop_update(true).await
}

#[tauri::command]
pub async fn application_install_update(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.inner().install_desktop_update().await
}

// ---------------------------------------------------------------------------
// Plugin marketplace
// ---------------------------------------------------------------------------

fn service_running(state: &AppState) -> bool {
    state.snapshot().phase == dsh_core::LauncherPhase::Ready
}

#[tauri::command]
pub fn market_get_catalog(state: State<'_, Arc<AppState>>) -> MarketCatalogState {
    state.inner().marketplace.catalog_state()
}

#[tauri::command]
pub async fn market_refresh_catalog(
    state: State<'_, Arc<AppState>>,
) -> Result<MarketCatalogState, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.refresh()).await,
    )
}

#[tauri::command]
pub async fn market_refresh_if_stale(
    state: State<'_, Arc<AppState>>,
) -> Result<MarketCatalogState, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.refresh_if_stale()).await,
    )
}

#[tauri::command]
pub async fn market_query(
    query: MarketQuery,
    state: State<'_, Arc<AppState>>,
) -> Result<MarketPage, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.query(&query)).await,
    )
}

#[tauri::command]
pub async fn market_installed(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<InstalledPlugin>, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.installed()).await,
    )
}

#[tauri::command]
pub async fn market_compatibility(
    plugin_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<CompatibilityInfo, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.compatibility(&plugin_id))
            .await,
    )
}

#[tauri::command]
pub async fn market_compatibility_batch(
    plugin_ids: Vec<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<dsh_core::marketplace::PluginCompatibility>, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || {
            state.marketplace.compatibility_batch(&plugin_ids)
        })
        .await,
    )
}

#[tauri::command]
pub async fn market_inspect(
    plugin_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<PluginSummary, AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.inspect(&plugin_id)).await,
    )
}

#[tauri::command]
pub async fn market_install(
    plugin_id: String,
    force: bool,
    expected_version: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<MarketOperationResult, AppError> {
    let running = service_running(state.inner());
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || {
            state
                .marketplace
                .install(&plugin_id, force, expected_version.as_deref(), running)
        })
        .await,
    )
}

#[tauri::command]
pub async fn market_uninstall(
    plugin_id: String,
    target: Option<InstalledPlugin>,
    state: State<'_, Arc<AppState>>,
) -> Result<MarketOperationResult, AppError> {
    let running = service_running(state.inner());
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || {
            state
                .marketplace
                .uninstall(&plugin_id, target.as_ref(), running)
        })
        .await,
    )
}

#[tauri::command]
pub fn market_pending_verification(
    state: State<'_, Arc<AppState>>,
) -> Result<Option<PendingVerification>, AppError> {
    state.inner().marketplace.pending_verification()
}

#[tauri::command]
pub async fn market_rollback_pending(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.marketplace.rollback_pending()).await,
    )
}

// ---------------------------------------------------------------------------
// Remote access
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn remote_set_master(
    enabled: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.set_remote_master(enabled)).await,
    )
}

#[tauri::command]
pub async fn remote_set_lan_enabled(
    enabled: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.set_remote_lan_enabled(enabled)).await,
    )
}

#[tauri::command]
pub async fn remote_refresh_lan(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    flatten_blocking_result(
        tauri::async_runtime::spawn_blocking(move || state.refresh_remote_lan()).await,
    )
}

/// Enabling public access requires the UI's disclaimer acknowledgement; the
/// backend enforces it and answers `remoteDisclaimerRequired` otherwise.
#[tauri::command]
pub fn remote_set_public_enabled(
    enabled: bool,
    acknowledged: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.set_remote_public_enabled(enabled, acknowledged)
}

#[tauri::command]
pub fn remote_rotate_password(
    scope: dsh_core::RemoteScope,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.rotate_remote_password(scope)
}

/// Sets a user-chosen 8-digit password; `remotePasswordInvalid` otherwise.
/// Changing the password revokes every session of the scope.
#[tauri::command]
pub fn remote_set_password(
    scope: dsh_core::RemoteScope,
    password: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    state.set_remote_password(scope, password)
}

/// Re-runs the tunnel bootstrap after a failure (no toggle dance required).
#[tauri::command]
pub fn remote_retry_public(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.retry_remote_public()
}

/// SVG markup of the scope's QR code; `remoteUnavailable` while the scope
/// has no active URL to encode.
#[tauri::command]
pub fn remote_qr(
    scope: dsh_core::RemoteScope,
    state: State<'_, Arc<AppState>>,
) -> Result<String, AppError> {
    state.remote_qr_svg(scope)
}

/// Flatten the task transport result without changing an application error.
/// Only a failed/panicked blocking task is adapted to `serviceControlFailed`;
/// `Ok(Err(AppError))` keeps its original localizable code and values.
fn flatten_blocking_result<T, E>(result: Result<Result<T, AppError>, E>) -> Result<T, AppError>
where
    E: std::fmt::Display,
{
    result.map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub fn market_open_plugin_github(
    plugin_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), AppError> {
    if !valid_github_repo_id(&plugin_id) {
        return Err(AppError::new("externalLinkInvalid"));
    }
    let url = format!("https://github.com/{plugin_id}");
    state.inner().open_https_url(&url)
}

/// Catalog plugin ids are `owner/repo` GitHub references. Validate the shape
/// instead of banning `..` wholesale (valid repo names like `repo..name`
/// would be rejected otherwise). Scheme safety comes from the https allowlist
/// in `open_https_url`.
fn valid_github_repo_id(id: &str) -> bool {
    let Some((owner, repo)) = id.split_once('/') else {
        return false;
    };
    if repo.contains('/') {
        return false;
    }
    let valid_part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    valid_part(owner) && valid_part(repo)
}

#[cfg(test)]
mod tests {
    use super::flatten_blocking_result;
    use dsh_core::AppError;

    #[test]
    fn blocking_adapter_preserves_inner_application_error_codes() {
        let inner = AppError::new("marketProfileInvalid")
            .value("plugin", "example")
            .detail("candidate rejected");
        let result: Result<Result<(), AppError>, &str> = Ok(Err(inner.clone()));

        assert_eq!(flatten_blocking_result(result), Err(inner));
    }

    #[test]
    fn blocking_adapter_wraps_only_task_transport_failures() {
        let result: Result<Result<(), AppError>, &str> = Err("worker panicked");
        let error = flatten_blocking_result(result).expect_err("transport failure");

        assert_eq!(error.code, "serviceControlFailed");
        assert_eq!(error.safe_detail.as_deref(), Some("worker panicked"));
    }
}
