use std::sync::Arc;

use dsh_core::{
    AppError, HarnessUpdateMode, Language, LauncherSnapshot, ThemePreference,
    marketplace::{
        CompatibilityInfo, InstalledPlugin, MarketCatalogState, MarketOperationResult, MarketPage,
        MarketQuery, PendingVerification, PluginSummary,
    },
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
pub async fn launcher_stop(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.stop_service())
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn launcher_restart(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.restart_service())
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
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
    tauri::async_runtime::spawn_blocking(move || state.marketplace.refresh())
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_refresh_if_stale(
    state: State<'_, Arc<AppState>>,
) -> Result<MarketCatalogState, AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.marketplace.refresh_if_stale())
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_query(
    query: MarketQuery,
    state: State<'_, Arc<AppState>>,
) -> Result<MarketPage, AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.marketplace.query(&query))
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_installed(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<InstalledPlugin>, AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.marketplace.installed())
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_compatibility(
    plugin_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<CompatibilityInfo, AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.marketplace.compatibility(&plugin_id))
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_inspect(
    plugin_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<PluginSummary, AppError> {
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || state.marketplace.inspect(&plugin_id))
        .await
        .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
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
    tauri::async_runtime::spawn_blocking(move || {
        state
            .marketplace
            .install(&plugin_id, force, expected_version.as_deref(), running)
    })
    .await
    .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub async fn market_uninstall(
    plugin_id: String,
    target: Option<InstalledPlugin>,
    state: State<'_, Arc<AppState>>,
) -> Result<MarketOperationResult, AppError> {
    let running = service_running(state.inner());
    let state = Arc::clone(state.inner());
    tauri::async_runtime::spawn_blocking(move || {
        state
            .marketplace
            .uninstall(&plugin_id, target.as_ref(), running)
    })
    .await
    .map_err(|error| AppError::new("serviceControlFailed").detail(error.to_string()))?
}

#[tauri::command]
pub fn market_pending_verification(
    state: State<'_, Arc<AppState>>,
) -> Result<Option<PendingVerification>, AppError> {
    state.inner().marketplace.pending_verification()
}

#[tauri::command]
pub fn market_clear_pending_verification(state: State<'_, Arc<AppState>>) -> Result<(), AppError> {
    state.inner().marketplace.clear_pending_verification()
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
