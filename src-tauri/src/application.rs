use std::{
    fs::{File, OpenOptions},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use dsh_core::{
    ActivityCode, ActivityState, AppError, AppResult, ApplicationPaths, DesktopUpdateState,
    HarnessUpdateChannel, HarnessUpdateMode, HarnessUpdateState, Language, LauncherPhase,
    LauncherSnapshot, LauncherStep, MigrationState, ProgressState, ProxyMode, ProxySettings,
    ThemePreference,
    balance::{BalanceService, BalanceSnapshot},
    browser::BrowserCatalog,
    marketplace::{MarketOperationResult, MarketService, Marketplace},
    migration::MigrationService,
    network,
    pet::{PetListener, PetPreferencesPatch, PetSnapshot},
    preferences::Preferences,
    runtime::{
        DeploymentController, DeploymentEvent, activate_prepared_harness_update, deploy_runtime,
        discard_prepared_harness_update, installed_version, latest_harness_version,
        prepare_harness_update, previous_harness_version, recover_prepared_harness_update,
        rollback_harness_runtime,
    },
    service::ServerManager,
    startup_repair::{
        ProjectionCacheRepair, StartupRepairBackupSummary, clear_startup_repair_backups,
        prune_startup_repair_backups, startup_repair_backup_summary,
    },
    terminal::ensure_terminal_command,
};
use fs2::{FileExt, lock_contended_error};
use semver::Version;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{TrayIcon, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

use crate::commands;

const WEBSITE: &str = "https://dsdesktop.com/";
const GITHUB_REPOSITORY: &str = "https://github.com/Gru110110110/deepseek-harness-desktop-launcher";
const HARNESS_GITHUB_REPOSITORY: &str = "https://github.com/deepseek-ai/deepseek-harness";
const DEEPSEEK_PLATFORM: &str = "https://platform.deepseek.com/";
const INSTANCE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_UPDATE_TIMEOUT: Duration = Duration::from_secs(30);
const DESKTOP_UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS: usize = 3;
const DESKTOP_UPDATE_RETRY_DELAY: Duration = Duration::from_millis(750);
const HARNESS_UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct ProgressEventThrottle {
    last_emit: Option<Instant>,
}

impl ProgressEventThrottle {
    fn reset(&mut self) {
        self.last_emit = None;
    }

    fn should_emit(&mut self, done: u64, total: Option<u64>, now: Instant) -> bool {
        let complete = total.is_some_and(|total| total > 0 && done >= total);
        let due = self
            .last_emit
            .is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_EVENT_INTERVAL);
        if complete || due {
            self.last_emit = Some(now);
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct DownloadProgressThrottle {
    done: u64,
    events: ProgressEventThrottle,
}

#[derive(Default)]
struct MainWindowActivation {
    pending: AtomicBool,
}

impl MainWindowActivation {
    fn request(&self) {
        self.pending.store(true, Ordering::SeqCst);
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

impl DownloadProgressThrottle {
    fn record(&mut self, chunk: usize, total: Option<u64>, now: Instant) -> Option<u64> {
        self.done = self.done.saturating_add(chunk as u64);
        self.events
            .should_emit(self.done, total, now)
            .then_some(self.done)
    }
}

fn external_link_url(target: &str) -> Option<&'static str> {
    match target {
        "github" => Some(GITHUB_REPOSITORY),
        "harnessGithub" => Some(HARNESS_GITHUB_REPOSITORY),
        "deepseek" => Some(DEEPSEEK_PLATFORM),
        _ => None,
    }
}

/// How the unified proxy configuration maps onto the updater. `Direct` uses
/// the builder's `no_proxy`; `Manual` goes through `configure_client` so the
/// bypass list applies to update checks and downloads alike. `System` only
/// uses `configure_client` when the Windows takeover decision yields a
/// non-empty merged plan (see `dsh_core::network::takeover_system_plan`);
/// on macOS/Linux, or whenever nothing explicit resolved, the plugin's
/// default builder is kept so reqwest merges environment variables with the
/// OS system proxy itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdaterProxyPlan {
    System,
    Direct,
    Manual {
        url: url::Url,
        bypass: Option<String>,
    },
}

fn updater_proxy_plan(settings: &ProxySettings) -> AppResult<UpdaterProxyPlan> {
    match settings.mode {
        ProxyMode::System => Ok(UpdaterProxyPlan::System),
        ProxyMode::Direct => Ok(UpdaterProxyPlan::Direct),
        ProxyMode::Manual => {
            // Reuse the core validation so the updater never receives a URL
            // the rest of the launcher already rejected (bad scheme, empty
            // host, non-root path, or embedded credentials).
            network::validate(settings)?;
            let url = url::Url::parse(settings.url.trim())
                .map_err(|_| AppError::new("proxyUrlInvalid").value("reason", "invalid"))?;
            Ok(UpdaterProxyPlan::Manual {
                url,
                bypass: network::normalized_bypass(&settings.bypass),
            })
        }
    }
}

/// Builds the updater-plugin (reqwest 0.13) proxy for manual mode, including
/// the normalized bypass list.
fn updater_manual_proxy(url: &url::Url, bypass: Option<&str>) -> AppResult<reqwest_updater::Proxy> {
    let proxy = reqwest_updater::Proxy::all(url.as_str())
        .map_err(|_| AppError::new("proxyUrlInvalid").value("reason", "invalid"))?;
    Ok(
        match bypass.and_then(reqwest_updater::NoProxy::from_string) {
            Some(no_proxy) => proxy.no_proxy(Some(no_proxy)),
            None => proxy,
        },
    )
}

/// Applies a resolved system plan to the updater's reqwest 0.13 builder.
/// Mirrors `dsh_core::network::apply_system_plan`: disables the plugin's own
/// env/system reading and installs the merged per-protocol proxies, so the
/// broken per-protocol `ProxyServer` handling of hyper-util's Windows matcher
/// is never in play. Test-only: production never hands an empty plan to
/// explicit takeover (see `takeover_system_plan`); it builds proxies up front
/// via [`updater_system_proxies`] and applies them inside `configure_client`,
/// while an empty or non-Windows resolution keeps the default builder.
#[cfg(test)]
fn apply_updater_system_plan(
    builder: reqwest_updater::ClientBuilder,
    plan: &network::SystemProxyPlan,
) -> AppResult<reqwest_updater::ClientBuilder> {
    let proxies = updater_system_proxies(plan)?;
    if proxies.is_empty() {
        return Ok(builder);
    }
    let mut builder = builder.no_proxy();
    for proxy in proxies {
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

/// Builds the reqwest 0.13 proxies for a resolved system plan, with the
/// normalized bypass list attached to each. Called only for a non-empty
/// Windows takeover plan; the resulting proxies are installed inside
/// `configure_client`, which also disables the plugin's own env/system
/// reading.
fn updater_system_proxies(
    plan: &network::SystemProxyPlan,
) -> AppResult<Vec<reqwest_updater::Proxy>> {
    let invalid = |error: reqwest_updater::Error| {
        AppError::new("desktopUpdateConfigurationInvalid")
            .detail(network::sanitize_detail(&error.to_string()))
    };
    let no_proxy = plan
        .no_proxy
        .as_deref()
        .and_then(reqwest_updater::NoProxy::from_string);
    let mut proxies = Vec::new();
    if let Some(http) = &plan.http {
        proxies.push(
            reqwest_updater::Proxy::http(http)
                .map_err(invalid)?
                .no_proxy(no_proxy.clone()),
        );
    }
    if let Some(https) = &plan.https {
        proxies.push(
            reqwest_updater::Proxy::https(https)
                .map_err(invalid)?
                .no_proxy(no_proxy),
        );
    }
    Ok(proxies)
}

/// Sanitized one-line form of an updater error for logs. Transport details
/// can embed URLs, and proxy credentials inherited from the environment must
/// never reach the log, so everything passes through the core sanitizer and
/// the raw `Debug` form is never logged.
fn updater_error_log(error: &tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;
    match error {
        Error::Reqwest(transport) => format!(
            "request failed: {}",
            network::sanitize_detail(&transport.to_string())
        ),
        other => network::sanitize_detail(&other.to_string()),
    }
}

pub(crate) struct AppState {
    app: AppHandle,
    paths: ApplicationPaths,
    _instance_lock: File,
    snapshot: Mutex<LauncherSnapshot>,
    preferences: Mutex<Preferences>,
    proxy_update: Mutex<()>,
    browsers: BrowserCatalog,
    server: Mutex<ServerManager>,
    pet_snapshot: Arc<Mutex<PetSnapshot>>,
    balance: BalanceService,
    deployment: Mutex<Option<DeploymentController>>,
    background_update: Mutex<Option<DeploymentController>>,
    migration: MigrationService,
    pub(crate) marketplace: Marketplace,
    remote: Arc<dsh_core::remote::RemoteService>,
    desktop_update_busy: AtomicBool,
    harness_update_check_busy: AtomicBool,
    startup_thread: Mutex<Option<thread::JoinHandle<()>>>,
    startup_repair_backups: Mutex<()>,
    background_update_thread: Mutex<Option<thread::JoinHandle<()>>>,
    quitting: AtomicBool,
    exit_ready: AtomicBool,
    tray: Mutex<Option<TrayIcon>>,
}

impl AppState {
    fn new(app: AppHandle, paths: ApplicationPaths) -> AppResult<Arc<Self>> {
        let instance_lock = acquire_instance_lock(&paths)?;
        let preferences = Preferences::load(&paths.preferences_file, &paths.language_file);
        // The unified proxy configuration applies to every Launcher-owned
        // network operation from the start, before any update check runs.
        network::activate(preferences.proxy.clone());
        let browsers = BrowserCatalog::discover();
        let mut snapshot = LauncherSnapshot::initial(env!("CARGO_PKG_VERSION"));
        snapshot.language = preferences.language;
        snapshot.theme = preferences.theme;
        snapshot.show_balance_card = preferences.show_balance_card;
        snapshot.harness_update_channel = preferences.harness_update_channel;
        snapshot.proxy = preferences.proxy.clone();
        snapshot.pet = preferences.pet.clone();
        snapshot.browsers = browsers.choices();
        snapshot.selected_browser_id = if browsers.contains(&preferences.browser_id) {
            preferences.browser_id.clone()
        } else {
            "system".into()
        };
        snapshot.harness_version = installed_version(&paths);
        snapshot.previous_harness_version = previous_harness_version(&paths);
        match recover_prepared_harness_update(&paths) {
            Ok(Some(version)) => {
                snapshot.harness_update = HarnessUpdateState::Downloaded { version };
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("prepared Harness update could not be resumed: {error}");
            }
        }
        let migration = MigrationService::from_environment(paths.clone())?;
        let marketplace = Marketplace::new(paths.clone());
        marketplace.initialize();
        let remote = dsh_core::remote::RemoteService::new(paths.clone())?;
        snapshot.remote = remote.snapshot();
        let pet_snapshot = Arc::new(Mutex::new(PetSnapshot::default()));
        let pet_snapshot_for_listener = Arc::clone(&pet_snapshot);
        let pet_app = app.clone();
        let pet_listener: PetListener = Arc::new(move |value| {
            *pet_snapshot_for_listener
                .lock()
                .expect("pet snapshot poisoned") = value.clone();
            let _ = pet_app.emit("pet://state", value);
        });
        let state = Arc::new(Self {
            app,
            _instance_lock: instance_lock,
            server: Mutex::new(ServerManager::with_pet_listener(
                paths.clone(),
                pet_listener,
            )),
            pet_snapshot,
            balance: BalanceService::new(),
            paths,
            snapshot: Mutex::new(snapshot),
            preferences: Mutex::new(preferences),
            proxy_update: Mutex::new(()),
            browsers,
            deployment: Mutex::new(None),
            background_update: Mutex::new(None),
            migration,
            marketplace,
            remote,
            desktop_update_busy: AtomicBool::new(false),
            harness_update_check_busy: AtomicBool::new(false),
            startup_thread: Mutex::new(None),
            startup_repair_backups: Mutex::new(()),
            background_update_thread: Mutex::new(None),
            quitting: AtomicBool::new(false),
            exit_ready: AtomicBool::new(false),
            tray: Mutex::new(None),
        });
        let weak_state = Arc::downgrade(&state);
        state.marketplace.set_operation_listener(move |busy| {
            if let Some(state) = weak_state.upgrade() {
                state.market_operation_changed(busy);
            }
        });
        let weak_state = Arc::downgrade(&state);
        state.marketplace.set_catalog_listener(move || {
            if let Some(state) = weak_state.upgrade() {
                state.market_catalog_changed();
            }
        });
        let weak_state = Arc::downgrade(&state);
        state.remote.set_change_listener(Box::new(move || {
            if let Some(state) = weak_state.upgrade() {
                state.remote_changed();
            }
        }));
        Ok(state)
    }

    pub(crate) fn snapshot(&self) -> LauncherSnapshot {
        self.snapshot.lock().expect("snapshot poisoned").clone()
    }

    pub(crate) fn pet_snapshot(&self) -> PetSnapshot {
        self.pet_snapshot
            .lock()
            .expect("pet snapshot poisoned")
            .clone()
    }

    pub(crate) fn startup_repair_backup_summary(&self) -> AppResult<StartupRepairBackupSummary> {
        let _guard = self
            .startup_repair_backups
            .lock()
            .expect("startup repair backups poisoned");
        startup_repair_backup_summary(&self.paths)
    }

    pub(crate) fn clear_startup_repair_backups(&self) -> AppResult<StartupRepairBackupSummary> {
        if matches!(
            self.snapshot().phase,
            LauncherPhase::Preparing | LauncherPhase::Starting | LauncherPhase::Stopping
        ) {
            return Err(AppError::new("launcherBusy"));
        }
        let _guard = self
            .startup_repair_backups
            .lock()
            .expect("startup repair backups poisoned");
        clear_startup_repair_backups(&self.paths)
    }

    pub(crate) fn acknowledge_startup_repair(&self) {
        self.mutate_if(clear_startup_repair_notice);
    }

    fn prune_startup_repair_backups_after_healthy_start(&self) {
        let _guard = self
            .startup_repair_backups
            .lock()
            .expect("startup repair backups poisoned");
        if let Err(error) = prune_startup_repair_backups(&self.paths) {
            log::warn!("startup repair backup retention could not be applied: {error}");
        }
    }

    fn mutate(&self, update: impl FnOnce(&mut LauncherSnapshot)) {
        let _ = self.mutate_if(|snapshot| {
            update(snapshot);
            true
        });
    }

    fn mutate_if(&self, update: impl FnOnce(&mut LauncherSnapshot) -> bool) -> bool {
        let (value, previous_web_url) = {
            let mut snapshot = self.snapshot.lock().expect("snapshot poisoned");
            let previous_web_url = snapshot.web_url.clone();
            if !update(&mut snapshot) {
                return false;
            }
            snapshot.revision = snapshot.revision.saturating_add(1);
            (snapshot.clone(), previous_web_url)
        };
        let web_url_changed = value.web_url != previous_web_url;
        let web_url = value.web_url.clone();
        let _ = self.app.emit("launcher://state", value);
        if web_url_changed {
            // The remote proxies resolve the upstream per connection, so a
            // Harness restart never interrupts them; they only need to know
            // the new authority.
            if let Err(error) = self.remote.set_upstream(web_url.as_deref()) {
                log::warn!("remote upstream update failed: {error}");
            }
        }
        true
    }

    /// Mirrors the remote service state into the launcher snapshot. The
    /// service notifies after releasing its own locks, so re-entering it
    /// here cannot deadlock.
    fn remote_changed(&self) {
        let remote = self.remote.snapshot();
        self.mutate(|snapshot| snapshot.remote = remote);
    }

    fn market_operation_changed(&self, busy: bool) {
        let _ = self.mutate_if(|snapshot| update_market_operation_state(snapshot, busy));
    }

    fn market_catalog_changed(&self) {
        self.mutate(|snapshot| {
            snapshot.market_catalog_revision = snapshot.market_catalog_revision.saturating_add(1);
        });
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        force: bool,
        target_version: Option<String>,
    ) -> AppResult<()> {
        let snapshot = self.snapshot();
        if !force && let HarnessUpdateState::Downloaded { version } = snapshot.harness_update {
            return self.start_worker(
                true,
                Some(version),
                false,
                true,
                snapshot.phase != LauncherPhase::Stopped,
            );
        }
        self.start_worker(
            force,
            target_version,
            false,
            false,
            snapshot.phase != LauncherPhase::Stopped,
        )
    }

    pub(crate) fn run_market_operation(
        &self,
        operation: impl FnOnce(
            &Marketplace,
            bool,
            &mut dyn MarketService,
        ) -> AppResult<MarketOperationResult>,
    ) -> AppResult<MarketOperationResult> {
        let _guard = self.marketplace.begin_operation()?;
        let phase = self.snapshot().phase;
        if self.quitting.load(Ordering::SeqCst)
            || !matches!(
                phase,
                LauncherPhase::Ready | LauncherPhase::Stopped | LauncherPhase::Failed
            )
        {
            return Err(AppError::new("launcherBusy"));
        }
        let defer_restart = self.marketplace.has_pending_web_rollback()
            || !self
                .server
                .lock()
                .expect("server poisoned")
                .can_restart_automatically();
        // Keep the gate through activation/restoration, or return a durable
        // pending result without restarting a host whose work must be preserved.
        operation(
            &self.marketplace,
            phase == LauncherPhase::Ready,
            &mut MarketActivationService {
                app: self,
                defer_restart,
            },
        )
    }

    pub(crate) fn stop_service(&self) -> AppResult<()> {
        let _market_guard = self.marketplace.begin_operation()?;
        if !self.mutate_if(|snapshot| {
            if snapshot.phase != LauncherPhase::Ready {
                return false;
            }
            snapshot.phase = LauncherPhase::Stopping;
            true
        }) {
            return Err(AppError::new("serviceNotReady"));
        }
        match self.server.lock().expect("server poisoned").stop() {
            Ok(()) => {
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Stopped;
                    snapshot.web_url = None;
                    snapshot.service_started_at_ms = None;
                    snapshot.activity = None;
                    snapshot.error = None;
                });
                Ok(())
            }
            Err(error) => {
                self.fail(error.clone());
                Err(error)
            }
        }
    }

    pub(crate) fn restart_service(&self) -> AppResult<()> {
        let _market_guard = self.marketplace.begin_operation()?;
        if !self.mutate_if(|snapshot| {
            if snapshot.phase != LauncherPhase::Ready {
                return false;
            }
            snapshot.phase = LauncherPhase::Starting;
            snapshot.web_url = None;
            snapshot.service_started_at_ms = None;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::StartingService,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
            true
        }) {
            return Err(AppError::new("serviceNotReady"));
        }
        let had_pending_market_change = self.marketplace.has_pending_web_rollback();
        let restarted = {
            let mut server = self.server.lock().expect("server poisoned");
            if let Err(error) = server.stop() {
                self.fail(error.clone());
                return Err(error);
            }
            server.start().and_then(|url| {
                if had_pending_market_change {
                    server.verify_web_ready(&url, || self.quitting.load(Ordering::SeqCst))?;
                }
                Ok(url)
            })
        };
        match restarted {
            Ok(url) => {
                if let Err(error) = self
                    .marketplace
                    .clear_web_pending_verification_while_guarded()
                {
                    log::warn!("could not clear verified marketplace rollback state: {error}");
                }
                self.prune_startup_repair_backups_after_healthy_start();
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.activity = None;
                    snapshot.error = None;
                });
                Ok(())
            }
            Err(error)
                if had_pending_market_change
                    && should_rollback_marketplace_after_start_failure(&error, false) =>
            {
                let plugins = self.marketplace.pending_web_change_summary();
                log::error!(
                    "Harness did not publish an address with an unverified marketplace batch; rolling back: {error}"
                );
                if let Err(stop_error) = self.server.lock().expect("server poisoned").stop() {
                    self.fail(stop_error.clone());
                    return Err(stop_error);
                }
                if let Err(rollback_error) = self.marketplace.rollback_web_pending_while_guarded() {
                    self.fail(rollback_error.clone());
                    return Err(rollback_error);
                }
                match self.server.lock().expect("server poisoned").start() {
                    Ok(url) => {
                        self.prune_startup_repair_backups_after_healthy_start();
                        let failure = AppError::new("marketVerificationFailed")
                            .value("plugins", plugins)
                            .detail(error.safe_detail.unwrap_or(error.code));
                        self.mutate(|snapshot| {
                            snapshot.phase = LauncherPhase::Ready;
                            snapshot.web_url = Some(url);
                            snapshot.service_started_at_ms = Some(now_ms());
                            snapshot.activity = None;
                            snapshot.error = Some(failure.clone());
                        });
                        Err(failure)
                    }
                    Err(restore_error) => {
                        self.fail(restore_error.clone());
                        Err(restore_error)
                    }
                }
            }
            Err(error) => {
                self.fail(error.clone());
                Err(error)
            }
        }
    }

    pub(crate) fn rollback_harness(
        self: &Arc<Self>,
        expected_version: String,
    ) -> AppResult<String> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err(AppError::new("deploymentCancelled"));
        }
        {
            let mut slot = self.startup_thread.lock().expect("startup thread poisoned");
            if slot.as_ref().is_some_and(|worker| !worker.is_finished()) {
                return Err(AppError::new("launcherBusy"));
            }
            if let Some(finished) = slot.take()
                && finished.join().is_err()
            {
                return Err(AppError::new("launcherWorkerFailed"));
            }
        }
        let _market_guard = self.marketplace.begin_operation()?;
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            LauncherPhase::Failed | LauncherPhase::Stopped
        ) {
            return Err(AppError::new("serviceNotStopped"));
        }
        if snapshot.previous_harness_version.as_deref() != Some(expected_version.as_str()) {
            return Err(AppError::new("previousHarnessVersionChanged")
                .value("expected", expected_version)
                .value(
                    "actual",
                    snapshot.previous_harness_version.unwrap_or_default(),
                ));
        }
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Preparing;
            snapshot.step = LauncherStep::Prepare;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::CheckingRuntime,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
            snapshot.progress = ProgressState::Indeterminate;
            snapshot.error = None;
            snapshot.removed_incompatible_plugins.clear();
            snapshot.repaired_projection_cache = false;
        });
        if let Err(error) = self.server.lock().expect("server poisoned").stop() {
            self.fail(error.clone());
            return Err(error);
        }
        let version = match rollback_harness_runtime(&self.paths, &expected_version) {
            Ok(version) => version,
            Err(error) => {
                self.mutate(|snapshot| {
                    snapshot.harness_version = installed_version(&self.paths);
                    snapshot.previous_harness_version = previous_harness_version(&self.paths);
                });
                self.fail(error.clone());
                return Err(error);
            }
        };
        let previous = previous_harness_version(&self.paths);
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Starting;
            snapshot.step = LauncherStep::Start;
            snapshot.harness_version = Some(version.clone());
            snapshot.previous_harness_version = previous;
            snapshot.harness_update = HarnessUpdateState::None;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::StartingService,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
        });
        match self.server.lock().expect("server poisoned").start() {
            Ok(url) => {
                self.prune_startup_repair_backups_after_healthy_start();
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.activity = None;
                    snapshot.error = None;
                });
                let state = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    let _ = state.check_harness_update().await;
                });
                Ok(version)
            }
            Err(error) => {
                self.fail(error.clone());
                Err(error)
            }
        }
    }

    /// Isolate only log-derived projection cache state, transactionally remove
    /// loader-identified third-party plugins, and retry the current runtime.
    /// Authoritative sessions, credentials, settings, and workspace data are
    /// never part of this repair surface.
    pub(crate) fn repair_and_start(self: &Arc<Self>) -> AppResult<()> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err(AppError::new("deploymentCancelled"));
        }
        {
            let mut slot = self.startup_thread.lock().expect("startup thread poisoned");
            if slot.as_ref().is_some_and(|worker| !worker.is_finished()) {
                return Err(AppError::new("launcherBusy"));
            }
            if let Some(finished) = slot.take()
                && finished.join().is_err()
            {
                return Err(AppError::new("launcherWorkerFailed"));
            }
        }
        let _market_guard = self.marketplace.begin_operation()?;
        let _repair_backup_guard = self
            .startup_repair_backups
            .lock()
            .expect("startup repair backups poisoned");
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            LauncherPhase::Failed | LauncherPhase::Stopped
        ) {
            return Err(AppError::new("serviceNotStopped"));
        }
        let startup_error = snapshot
            .error
            .clone()
            .ok_or_else(|| AppError::new("startupRepairUnavailable"))?;
        let plugins = incompatible_plugin_packages(&startup_error);
        let repairable_cache = startup_error
            .values
            .get("repairableProjectionCache")
            .is_some_and(|value| value == "true");
        if plugins.is_empty() && !repairable_cache {
            return Err(AppError::new("startupRepairUnavailable"));
        }

        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Starting;
            snapshot.step = LauncherStep::Start;
            snapshot.web_url = None;
            snapshot.service_started_at_ms = None;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::RepairingStartup,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
            snapshot.progress = ProgressState::Indeterminate;
            snapshot.error = None;
            snapshot.removed_incompatible_plugins.clear();
            snapshot.repaired_projection_cache = false;
        });
        if let Err(error) = self.server.lock().expect("server poisoned").stop() {
            self.fail(error.clone());
            return Err(error);
        }

        let cache_repair = match ProjectionCacheRepair::prepare(&self.paths) {
            Ok(repair) => repair,
            Err(error) => {
                self.fail(error.clone());
                return Err(error);
            }
        };
        let cache_changed = cache_repair.changed();
        let removed = if plugins.is_empty() {
            Vec::new()
        } else {
            match self
                .marketplace
                .remove_incompatible_profile_packages_while_guarded(&plugins)
            {
                Ok(removed) => removed,
                Err(error) => {
                    let plugin_rollback = if self.marketplace.has_pending_web_rollback() {
                        self.marketplace.rollback_web_pending_while_guarded()
                    } else {
                        Ok(())
                    };
                    let cache_rollback = cache_repair.restore();
                    let failure = plugin_rollback
                        .err()
                        .or_else(|| cache_rollback.err())
                        .unwrap_or(error);
                    self.fail(failure.clone());
                    return Err(failure);
                }
            }
        };

        self.mutate(|snapshot| {
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::StartingService,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
        });
        match self.server.lock().expect("server poisoned").start() {
            Ok(url) => {
                if let Err(error) = self
                    .marketplace
                    .clear_web_pending_verification_while_guarded()
                {
                    log::warn!("verified startup repair left marketplace cleanup work: {error}");
                }
                if let Err(error) = cache_repair.verify() {
                    // Publication already succeeded and the original cache is
                    // still retained. A marker-write failure is cleanup work,
                    // not grounds to take the healthy service back down.
                    log::warn!("verified startup repair backup marker was not updated: {error}");
                }
                if let Err(error) = prune_startup_repair_backups(&self.paths) {
                    log::warn!("startup repair backup retention could not be applied: {error}");
                }
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.activity = None;
                    snapshot.error = None;
                    snapshot.removed_incompatible_plugins = removed;
                    snapshot.repaired_projection_cache = cache_changed;
                });
                let state = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    let _ = state.check_harness_update().await;
                });
                Ok(())
            }
            Err(error) => {
                let plugin_rollback = if self.marketplace.has_pending_web_rollback() {
                    self.marketplace.rollback_web_pending_while_guarded()
                } else {
                    Ok(())
                };
                let cache_rollback = cache_repair.restore();
                let failure = plugin_rollback
                    .err()
                    .or_else(|| cache_rollback.err())
                    .unwrap_or(error);
                self.fail(failure.clone());
                Err(failure)
            }
        }
    }

    fn start_worker(
        self: &Arc<Self>,
        force: bool,
        target_version: Option<String>,
        migration_approved: bool,
        activate_prepared: bool,
        restore_service_on_failure: bool,
    ) -> AppResult<()> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err(AppError::new("deploymentCancelled"));
        }
        let market_guard = self.marketplace.begin_operation()?;
        let mut slot = self.startup_thread.lock().expect("startup thread poisoned");
        if slot.as_ref().is_some_and(|thread| !thread.is_finished()) {
            return Err(AppError::new("launcherBusy"));
        }
        if let Some(finished) = slot.take()
            && finished.join().is_err()
        {
            log::error!("launcher startup worker panicked before it was reaped");
        }
        let previous_update = self.snapshot().harness_update;
        let controller = DeploymentController::default();
        *self.deployment.lock().expect("deployment poisoned") = Some(controller.clone());
        if self.quitting.load(Ordering::SeqCst) {
            controller.cancel();
            *self.deployment.lock().expect("deployment poisoned") = None;
            return Err(AppError::new("deploymentCancelled"));
        }
        if force {
            self.mutate(|snapshot| {
                snapshot.harness_update = HarnessUpdateState::Installing {
                    version: target_version.clone().unwrap_or_else(|| "latest".into()),
                };
            });
        }
        let state = Arc::clone(self);
        let worker = thread::Builder::new()
            .name("launcher-startup".into())
            .spawn(move || {
                let _market_guard = market_guard;
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    state.run_startup(
                        force,
                        target_version,
                        migration_approved,
                        activate_prepared,
                        restore_service_on_failure,
                        &controller,
                    );
                }));
                *state.deployment.lock().expect("deployment poisoned") = None;
                if outcome.is_err() && !state.quitting.load(Ordering::SeqCst) {
                    state.fail(AppError::new("launcherWorkerFailed"));
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                *self.deployment.lock().expect("deployment poisoned") = None;
                let failure = AppError::new("launcherWorkerFailed").detail(error.to_string());
                self.mutate(|snapshot| {
                    fail_start_worker_spawn(snapshot, previous_update, failure.clone());
                });
                return Err(failure);
            }
        };
        *slot = Some(worker);
        Ok(())
    }

    fn run_startup(
        self: &Arc<Self>,
        force: bool,
        target_version: Option<String>,
        migration_approved: bool,
        activate_prepared: bool,
        restore_service_on_failure: bool,
        controller: &DeploymentController,
    ) {
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Preparing;
            snapshot.step = LauncherStep::Prepare;
            snapshot.error = None;
            snapshot.web_url = None;
            snapshot.service_started_at_ms = None;
            snapshot.removed_incompatible_plugins.clear();
            snapshot.repaired_projection_cache = false;
            snapshot.progress = ProgressState::Indeterminate;
            if force {
                snapshot.harness_update = HarnessUpdateState::Installing {
                    version: target_version.clone().unwrap_or_else(|| "latest".into()),
                };
            }
        });
        if !force {
            if let Err(error) = self.migration.recover() {
                self.fail_unless_quitting(error);
                return;
            }
            if migration_approved {
                self.mutate(|snapshot| {
                    snapshot.activity = Some(ActivityState {
                        code: ActivityCode::MigratingData,
                        values: Default::default(),
                        started_at_ms: now_ms(),
                    });
                    snapshot.progress = ProgressState::Indeterminate;
                });
                match self.migration.apply() {
                    Ok(outcome) => {
                        if let Some(warning) = outcome.warning {
                            log::warn!("optional CC Switch import was skipped: {warning}");
                            self.mutate(|snapshot| {
                                snapshot.migration =
                                    MigrationState::CompletedWithWarning { warning }
                            });
                        } else {
                            self.mutate(|snapshot| snapshot.migration = MigrationState::Completed);
                        }
                    }
                    Err(error) => {
                        log::warn!("local data import failed and will be skipped: {error:?}");
                        if let Err(skip_error) = self.migration.skip() {
                            log::error!(
                                "the failed import could not be safely recovered and skipped: {skip_error:?}"
                            );
                            self.fail_unless_quitting(skip_error);
                            return;
                        }
                        let detail = error.safe_detail.unwrap_or(error.code);
                        self.mutate(|snapshot| {
                            snapshot.migration = MigrationState::CompletedWithWarning {
                                warning: AppError::new("migrationImportSkipped").detail(detail),
                            }
                        });
                    }
                }
            } else {
                match self.migration.discover() {
                    Ok(Some(plan)) => {
                        self.mutate(|snapshot| {
                            snapshot.phase = LauncherPhase::AwaitingMigration;
                            snapshot.activity = None;
                            snapshot.progress = ProgressState::Indeterminate;
                            snapshot.migration = MigrationState::Pending { plan };
                        });
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.fail_unless_quitting(error);
                        return;
                    }
                }
            }
        }
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        if force && let Err(error) = self.server.lock().expect("server poisoned").stop() {
            self.fail_unless_quitting(error);
            return;
        }
        let weak = Arc::downgrade(self);
        let progress_events = Mutex::new(ProgressEventThrottle::default());
        let notify = move |event| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            match event {
                DeploymentEvent::Activity { code, values } => {
                    progress_events
                        .lock()
                        .expect("progress events poisoned")
                        .reset();
                    state.mutate(|snapshot| {
                        snapshot.activity = Some(ActivityState {
                            code,
                            values,
                            started_at_ms: now_ms(),
                        });
                        snapshot.progress = ProgressState::Indeterminate;
                    });
                }
                DeploymentEvent::Progress { done, total } => {
                    let publish = progress_events
                        .lock()
                        .expect("progress events poisoned")
                        .should_emit(done, total, Instant::now());
                    if publish {
                        state.mutate(|snapshot| {
                            snapshot.progress = total
                                .filter(|total| *total > 0)
                                .map_or(ProgressState::Indeterminate, |total| {
                                    ProgressState::Determinate { done, total }
                                })
                        });
                    }
                }
                DeploymentEvent::ActivityUpdate { values } => state.mutate(|snapshot| {
                    if let Some(activity) = snapshot.activity.as_mut() {
                        activity.values = values;
                    }
                }),
            }
        };
        let deployed = if activate_prepared {
            activate_prepared_harness_update(&self.paths, controller, notify)
        } else {
            let update_channel = self.snapshot().harness_update_channel;
            deploy_runtime(
                &self.paths,
                force,
                target_version.as_deref(),
                update_channel,
                controller,
                notify,
            )
        };
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        match deployed {
            Ok(version) => {
                let previous = previous_harness_version(&self.paths);
                self.mutate(|snapshot| {
                    complete_harness_deployment(snapshot, version, force);
                    snapshot.previous_harness_version = previous;
                });
                if let Err(error) = ensure_terminal_command(
                    &self.paths,
                    std::env::var_os("DSH_DESKTOP_HOME").is_none(),
                ) {
                    // Terminal integration is supplementary. A shell profile or
                    // PATH conflict must never roll back a healthy runtime or
                    // prevent the managed web service from starting.
                    log::warn!("the terminal dsh command could not be installed: {error:?}");
                }
            }
            Err(error) => {
                if force {
                    self.restore_service_after_failed_update(
                        error,
                        target_version.unwrap_or_else(|| "latest".into()),
                        restore_service_on_failure,
                    );
                } else {
                    self.fail_unless_quitting(error);
                }
                return;
            }
        }
        if force
            && !activate_prepared
            && let Err(error) = discard_prepared_harness_update(&self.paths)
        {
            log::warn!("obsolete prepared Harness update could not be discarded: {error}");
        }
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Starting;
            snapshot.step = LauncherStep::Start;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::StartingService,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
            snapshot.progress = ProgressState::Indeterminate;
        });
        let started = self
            .server
            .lock()
            .expect("server poisoned")
            .start_cancellable(|| {
                controller.is_cancelled() || self.quitting.load(Ordering::SeqCst)
            });
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        match started {
            Ok(url) => {
                if let Err(error) = self
                    .marketplace
                    .clear_web_pending_verification_while_guarded()
                {
                    log::warn!("could not clear verified marketplace rollback state: {error}");
                }
                self.prune_startup_repair_backups_after_healthy_start();
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.activity = None;
                    snapshot.error = None;
                });
                let state = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    let _ = state.check_harness_update().await;
                });
            }
            Err(error)
                if self.marketplace.has_pending_web_rollback()
                    && should_rollback_marketplace_after_start_failure(
                        &error,
                        force || activate_prepared,
                    ) =>
            {
                let plugins = self.marketplace.pending_web_change_summary();
                log::error!(
                    "Harness did not publish an address with an unverified marketplace batch; rolling back: {error}"
                );
                if let Err(rollback_error) = self.marketplace.rollback_web_pending_while_guarded() {
                    self.fail_unless_quitting(rollback_error);
                    return;
                }
                match self
                    .server
                    .lock()
                    .expect("server poisoned")
                    .start_cancellable(|| {
                        controller.is_cancelled() || self.quitting.load(Ordering::SeqCst)
                    }) {
                    Ok(url) => {
                        self.prune_startup_repair_backups_after_healthy_start();
                        let failure = AppError::new("marketVerificationFailed")
                            .value("plugins", plugins)
                            .detail(error.safe_detail.unwrap_or(error.code));
                        self.mutate(|snapshot| {
                            snapshot.phase = LauncherPhase::Ready;
                            snapshot.web_url = Some(url);
                            snapshot.service_started_at_ms = Some(now_ms());
                            snapshot.activity = None;
                            snapshot.error = Some(failure);
                        });
                    }
                    Err(restore_error) => self.fail_unless_quitting(restore_error),
                }
            }
            Err(error) => match self.try_recover_incompatible_plugins(&error, controller) {
                Some(Ok((url, removed))) => {
                    self.mutate(|snapshot| {
                        snapshot.phase = LauncherPhase::Ready;
                        snapshot.web_url = Some(url);
                        snapshot.service_started_at_ms = Some(now_ms());
                        snapshot.activity = None;
                        snapshot.error = None;
                        snapshot.removed_incompatible_plugins = removed;
                    });
                    let state = Arc::clone(self);
                    tauri::async_runtime::spawn(async move {
                        let _ = state.check_harness_update().await;
                    });
                }
                Some(Err(recovery_error)) => self.fail_unless_quitting(recovery_error),
                None => self.fail_unless_quitting(error),
            },
        }
    }

    /// If startup output names an installed third-party profile package as
    /// incompatible, uninstall it through the marketplace transaction and
    /// retry exactly once. The uninstall is committed only after the retry
    /// publishes a Web UI address; otherwise the original profile is restored.
    fn try_recover_incompatible_plugins(
        &self,
        startup_error: &AppError,
        controller: &DeploymentController,
    ) -> Option<AppResult<(String, Vec<String>)>> {
        if self.marketplace.has_pending_web_rollback() {
            return None;
        }
        let candidates = incompatible_plugin_packages(startup_error);
        if candidates.is_empty() {
            return None;
        }
        let removed = match self
            .marketplace
            .remove_incompatible_profile_packages_while_guarded(&candidates)
        {
            Ok(removed) if removed.is_empty() => return None,
            Ok(removed) => removed,
            Err(error) => {
                log::warn!(
                    "incompatible plugin recovery could not prepare a safe uninstall: {error}"
                );
                if self.marketplace.has_pending_web_rollback()
                    && let Err(rollback_error) =
                        self.marketplace.rollback_web_pending_while_guarded()
                {
                    return Some(Err(rollback_error));
                }
                return Some(Err(startup_error.clone()));
            }
        };
        log::warn!(
            "Harness startup named incompatible profile packages; retrying after transactional uninstall: {}",
            removed.join(", ")
        );
        let retried = self
            .server
            .lock()
            .expect("server poisoned")
            .start_cancellable(|| {
                controller.is_cancelled() || self.quitting.load(Ordering::SeqCst)
            });
        match retried {
            Ok(url) => {
                if let Err(error) = self
                    .marketplace
                    .clear_web_pending_verification_while_guarded()
                {
                    log::warn!("verified incompatible plugin recovery left cleanup work: {error}");
                }
                self.prune_startup_repair_backups_after_healthy_start();
                Some(Ok((url, removed)))
            }
            Err(error) => {
                log::warn!(
                    "Harness still failed after incompatible plugin removal; restoring plugins: {error}"
                );
                if let Err(rollback_error) = self.marketplace.rollback_web_pending_while_guarded() {
                    return Some(Err(rollback_error));
                }
                Some(Err(retain_incompatible_plugin_context(error, &removed)))
            }
        }
    }

    fn fail_unless_quitting(&self, error: AppError) {
        if !self.quitting.load(Ordering::SeqCst) {
            self.fail(error);
        }
    }

    fn fail(&self, error: AppError) {
        log::error!("launcher operation failed: {error:?}");
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Failed;
            snapshot.error = Some(error);
            snapshot.activity = None;
            if let HarnessUpdateState::Installing { version } = &snapshot.harness_update {
                snapshot.harness_update = HarnessUpdateState::Failed {
                    version: version.clone(),
                };
            }
        });
    }

    fn restore_service_after_failed_update(
        &self,
        update_error: AppError,
        version: String,
        restore_service: bool,
    ) {
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        log::error!("Harness update failed and was rolled back: {update_error}");
        if !restore_service {
            self.mutate(|snapshot| {
                snapshot.phase = LauncherPhase::Stopped;
                snapshot.step = LauncherStep::Start;
                snapshot.activity = None;
                snapshot.error = Some(update_error);
                snapshot.web_url = None;
                snapshot.service_started_at_ms = None;
                snapshot.harness_version = installed_version(&self.paths);
                snapshot.harness_update = HarnessUpdateState::Failed { version };
            });
            return;
        }
        let restarted = self.server.lock().expect("server poisoned").start();
        match restarted {
            Ok(url) => {
                self.prune_startup_repair_backups_after_healthy_start();
                self.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.step = LauncherStep::Start;
                    snapshot.activity = None;
                    snapshot.error = Some(update_error);
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.harness_version = installed_version(&self.paths);
                    snapshot.harness_update = HarnessUpdateState::Failed { version };
                });
            }
            Err(restart_error) => {
                log::error!("the previous Harness runtime could not be restarted: {restart_error}");
                self.fail(restart_error);
            }
        }
    }

    pub(crate) async fn check_harness_update(self: &Arc<Self>) -> AppResult<Option<String>> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err(AppError::new("harnessUpdateCheckCancelled"));
        }
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            LauncherPhase::Ready | LauncherPhase::Stopped
        ) {
            return Err(AppError::new("serviceNotReady"));
        }
        let _operation = self.begin_harness_update_check()?;
        let previous = snapshot.harness_update;
        if matches!(
            &previous,
            HarnessUpdateState::Downloading { .. } | HarnessUpdateState::Installing { .. }
        ) {
            return Err(AppError::new("harnessUpdateBusy"));
        }
        let current_value = snapshot
            .harness_version
            .ok_or_else(|| AppError::new("serviceNotReady"))?;
        let current = Version::parse(&current_value)
            .map_err(|_| AppError::new("runtimeVersionInvalid").value("version", current_value))?;
        if self.quitting.load(Ordering::SeqCst) {
            return Err(AppError::new("harnessUpdateCheckCancelled"));
        }
        if !self.mutate_if(|snapshot| mark_harness_update_checking(snapshot, &previous)) {
            return Err(AppError::new("harnessUpdateBusy"));
        }

        let channel = snapshot.harness_update_channel;
        let controller = DeploymentController::default();
        let query_controller = controller.clone();
        let query = tauri::async_runtime::spawn_blocking(move || {
            latest_harness_version(&query_controller, channel)
        });
        let result = match tokio::time::timeout(HARNESS_UPDATE_CHECK_TIMEOUT, query).await {
            Ok(joined) => joined
                .map_err(|error| AppError::new("versionQueryFailed").detail(error.to_string()))
                .and_then(|result| result),
            Err(_) => {
                controller.cancel();
                Err(AppError::new("harnessUpdateCheckTimedOut"))
            }
        };

        if self.quitting.load(Ordering::SeqCst) {
            controller.cancel();
            let _ =
                self.mutate_if(|snapshot| replace_harness_update_if_checking(snapshot, previous));
            return Err(AppError::new("harnessUpdateCheckCancelled"));
        }

        match result {
            Ok(latest) => {
                let latest = match Version::parse(&latest) {
                    Ok(latest) => latest,
                    Err(_) => {
                        let error =
                            AppError::new("runtimeVersionInvalid").value("version", &latest);
                        let _ = self.mutate_if(|snapshot| {
                            replace_harness_update_if_checking(snapshot, previous)
                        });
                        return Err(error);
                    }
                };
                let replacement = harness_update_after_check(previous, &current, &latest);
                let available = match &replacement {
                    HarnessUpdateState::Available { version }
                    | HarnessUpdateState::Downloaded { version }
                    | HarnessUpdateState::Failed { version } => Some(version.clone()),
                    _ => None,
                };
                let _ = self.mutate_if(|snapshot| {
                    replace_harness_update_if_checking(snapshot, replacement)
                });
                Ok(available)
            }
            Err(error) => {
                log::warn!("Harness update check failed: {error}");
                let _ = self
                    .mutate_if(|snapshot| replace_harness_update_if_checking(snapshot, previous));
                Err(error)
            }
        }
    }

    pub(crate) fn update_harness(
        self: &Arc<Self>,
        mode: HarnessUpdateMode,
        expected_version: String,
    ) -> AppResult<()> {
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            LauncherPhase::Ready | LauncherPhase::Stopped
        ) {
            return Err(AppError::new("serviceNotReady"));
        }
        let target = match snapshot.harness_update {
            HarnessUpdateState::Available { version } | HarnessUpdateState::Failed { version } => {
                Some(version)
            }
            _ => None,
        };
        let Some(target) = target else {
            return Err(AppError::new("harnessUpdateUnavailable"));
        };
        if target != expected_version {
            return Err(AppError::new("harnessUpdateChanged")
                .value("expected", expected_version)
                .value("actual", target));
        }
        match mode {
            HarnessUpdateMode::Foreground => self.start(true, Some(target))?,
            HarnessUpdateMode::Background => self.download_harness_update(target)?,
        }
        Ok(())
    }

    pub(crate) fn activate_harness_update(
        self: &Arc<Self>,
        expected_version: String,
    ) -> AppResult<()> {
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            LauncherPhase::Ready | LauncherPhase::Stopped
        ) {
            return Err(AppError::new("serviceNotReady"));
        }
        let target = match snapshot.harness_update {
            HarnessUpdateState::Downloaded { version } => version,
            _ => return Err(AppError::new("preparedHarnessUpdateUnavailable")),
        };
        if target != expected_version {
            return Err(AppError::new("harnessUpdateChanged")
                .value("expected", expected_version)
                .value("actual", target));
        }
        self.start_worker(
            true,
            Some(target),
            false,
            true,
            snapshot.phase == LauncherPhase::Ready,
        )
    }

    fn download_harness_update(self: &Arc<Self>, version: String) -> AppResult<()> {
        let mut slot = self
            .background_update_thread
            .lock()
            .expect("background update thread poisoned");
        if slot.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err(AppError::new("harnessUpdateBusy"));
        }
        if let Some(finished) = slot.take()
            && finished.join().is_err()
        {
            log::error!("background Harness update worker panicked before it was reaped");
        }
        let controller = DeploymentController::default();
        *self
            .background_update
            .lock()
            .expect("background update poisoned") = Some(controller.clone());
        if self.quitting.load(Ordering::SeqCst) {
            controller.cancel();
            *self
                .background_update
                .lock()
                .expect("background update poisoned") = None;
            return Err(AppError::new("deploymentCancelled"));
        }
        if !self.mutate_if(|snapshot| {
            if !matches!(
                snapshot.phase,
                LauncherPhase::Ready | LauncherPhase::Stopped
            ) || !matches!(
                &snapshot.harness_update,
                HarnessUpdateState::Available { version: available }
                    | HarnessUpdateState::Failed { version: available }
                    if available == &version
            ) {
                return false;
            }
            snapshot.error = None;
            snapshot.harness_update = HarnessUpdateState::Downloading {
                version: version.clone(),
            };
            true
        }) {
            *self
                .background_update
                .lock()
                .expect("background update poisoned") = None;
            return Err(AppError::new("harnessUpdateBusy"));
        }
        let state = Arc::clone(self);
        let failed_version = version.clone();
        let worker = thread::Builder::new()
            .name("harness-background-update".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    prepare_harness_update(&state.paths, &version, &controller, |_| {})
                }))
                .unwrap_or_else(|_| Err(AppError::new("backgroundHarnessUpdateFailed")));
                *state
                    .background_update
                    .lock()
                    .expect("background update poisoned") = None;
                if state.quitting.load(Ordering::SeqCst) {
                    return;
                }
                match result {
                    Ok(downloaded) => {
                        let _ = state.mutate_if(|snapshot| {
                            if !matches!(
                                &snapshot.harness_update,
                                HarnessUpdateState::Downloading { version: active }
                                    if active == &version
                            ) {
                                return false;
                            }
                            snapshot.harness_update = HarnessUpdateState::Downloaded {
                                version: downloaded,
                            };
                            true
                        });
                    }
                    Err(error) => {
                        log::warn!("background Harness update failed: {error}");
                        let _ = state.mutate_if(|snapshot| {
                            if !matches!(
                                &snapshot.harness_update,
                                HarnessUpdateState::Downloading { version: active }
                                    if active == &version
                            ) {
                                return false;
                            }
                            snapshot.error = Some(error.clone());
                            snapshot.harness_update = HarnessUpdateState::Failed {
                                version: version.clone(),
                            };
                            true
                        });
                    }
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                *self
                    .background_update
                    .lock()
                    .expect("background update poisoned") = None;
                let failure =
                    AppError::new("backgroundHarnessUpdateFailed").detail(error.to_string());
                self.mutate(|snapshot| {
                    snapshot.error = Some(failure.clone());
                    snapshot.harness_update = HarnessUpdateState::Failed {
                        version: failed_version,
                    };
                });
                return Err(failure);
            }
        };
        *slot = Some(worker);
        Ok(())
    }

    pub(crate) fn approve_migration(self: &Arc<Self>) -> AppResult<()> {
        let plan = match self.snapshot().migration {
            MigrationState::Pending { plan } => plan,
            _ => return Err(AppError::new("migrationNotAvailable")),
        };
        self.join_startup();
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Preparing;
            snapshot.migration = MigrationState::Applying { plan };
            snapshot.error = None;
        });
        self.start_worker(false, None, true, false, true)
    }

    pub(crate) fn skip_migration(self: &Arc<Self>) -> AppResult<()> {
        if !matches!(self.snapshot().migration, MigrationState::Pending { .. }) {
            return Err(AppError::new("migrationNotAvailable"));
        }
        self.join_startup();
        self.migration.skip()?;
        self.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Preparing;
            snapshot.migration = MigrationState::Skipped;
            snapshot.error = None;
        });
        self.start_worker(false, None, false, false, true)
    }

    pub(crate) fn set_language(&self, language: Language) -> AppResult<()> {
        {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.language = language;
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
        }
        self.mutate(|snapshot| snapshot.language = language);
        if let Err(error) = self.refresh_tray_menu(language) {
            log::warn!("tray language refresh failed: {error}");
        }
        Ok(())
    }
    pub(crate) fn set_theme(&self, theme: ThemePreference) -> AppResult<()> {
        {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.theme = theme;
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
        }
        self.mutate(|snapshot| snapshot.theme = theme);
        Ok(())
    }
    pub(crate) fn set_show_balance_card(&self, show: bool) -> AppResult<()> {
        {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.show_balance_card = show;
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
        }
        self.mutate(|snapshot| snapshot.show_balance_card = show);
        Ok(())
    }

    pub(crate) fn patch_pet_preferences(&self, patch: PetPreferencesPatch) -> AppResult<()> {
        let pet = {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.pet.apply_patch(patch)?;
            candidate.save(&self.paths.preferences_file)?;
            let pet = candidate.pet.clone();
            *preferences = candidate;
            pet
        };
        self.mutate(|snapshot| snapshot.pet = pet);
        if let Err(error) = self.refresh_tray_menu(self.snapshot().language) {
            log::warn!("tray pet refresh failed: {error}");
        }
        Ok(())
    }
    pub(crate) fn set_harness_update_channel(
        self: &Arc<Self>,
        channel: HarnessUpdateChannel,
    ) -> AppResult<()> {
        // Serialize channel changes with checks so a result resolved for the
        // old dist-tag can never be published after the new choice is saved.
        let _operation = self.begin_harness_update_check()?;
        let snapshot = self.snapshot();
        if snapshot.harness_update_channel == channel {
            return Ok(());
        }
        if matches!(
            snapshot.harness_update,
            HarnessUpdateState::Downloading { .. }
                | HarnessUpdateState::Downloaded { .. }
                | HarnessUpdateState::Installing { .. }
        ) {
            return Err(AppError::new("harnessUpdateBusy"));
        }
        {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.harness_update_channel = channel;
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
        }
        self.mutate(|snapshot| {
            snapshot.harness_update_channel = channel;
            // Available and failed candidates belong to the previously
            // selected dist-tag and must be resolved again.
            snapshot.harness_update = HarnessUpdateState::None;
        });
        Ok(())
    }
    /// Validates, atomically persists, and immediately activates a new proxy
    /// configuration. The next network operation and any "retry" use it
    /// without a restart; nothing about the runtime or user data is rebuilt
    /// when the new settings fail.
    pub(crate) fn set_proxy(&self, proxy: ProxySettings) -> AppResult<bool> {
        // Serialize persistence, activation, and snapshot publication so two
        // concurrent IPC calls cannot leave disk, the active client policy,
        // and the UI snapshot describing different proxy settings.
        let _update = self.proxy_update.lock().expect("proxy update poisoned");
        let proxy = network::for_persistence(proxy)?;
        let changed = {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let changed = preferences.proxy != proxy;
            let mut candidate = preferences.clone();
            candidate.proxy = proxy.clone();
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
            changed
        };
        network::activate(proxy.clone());
        self.mutate(|snapshot| snapshot.proxy = proxy);
        // Process environments are immutable. Launcher requests and newly
        // spawned subprocesses use the active settings immediately, but an
        // already-running Harness must be restarted by an explicit user
        // action. Return that fact rather than silently disrupting a session.
        let harness_restart_required = changed
            && self
                .server
                .lock()
                .expect("server manager poisoned")
                .is_running();
        Ok(harness_restart_required)
    }
    /// Tests the candidate proxy configuration from the settings form. The
    /// candidate is used as-is and never persisted by the test.
    pub(crate) fn test_proxy(&self, proxy: ProxySettings) -> AppResult<dsh_core::ProxyTestReport> {
        dsh_core::runtime::test_proxy_connection(&proxy)
    }
    /// Sanitized balance snapshot for the dashboard card. Only talks to the
    /// loopback bridge of the currently running service; the port, token,
    /// API key, and raw bridge output never leave this process.
    pub(crate) fn balance_snapshot(&self, force_refresh: bool) -> BalanceSnapshot {
        let endpoint = self
            .server
            .lock()
            .expect("server poisoned")
            .balance_endpoint();
        self.balance.snapshot(endpoint.as_ref(), force_refresh)
    }
    pub(crate) fn select_browser(&self, id: String) -> AppResult<()> {
        if !self.browsers.contains(&id) {
            return Err(AppError::new("browserUnavailable"));
        }
        {
            let mut preferences = self.preferences.lock().expect("preferences poisoned");
            let mut candidate = preferences.clone();
            candidate.browser_id = id.clone();
            candidate.save(&self.paths.preferences_file)?;
            *preferences = candidate;
        }
        self.mutate(|snapshot| snapshot.selected_browser_id = id);
        Ok(())
    }
    pub(crate) fn open_web_ui(&self) -> AppResult<()> {
        let snapshot = self.snapshot();
        let url = snapshot
            .web_url
            .ok_or_else(|| AppError::new("serviceNotReady"))?;
        self.browsers.open(&snapshot.selected_browser_id, &url)
    }
    pub(crate) fn open_website(&self) -> AppResult<()> {
        self.browsers.open("system", WEBSITE)
    }
    pub(crate) fn open_external_link(&self, target: &str) -> AppResult<()> {
        let url = external_link_url(target).ok_or_else(|| AppError::new("externalLinkInvalid"))?;
        self.browsers.open("system", url)
    }
    /// Open an https URL from market data after validating its origin against
    /// a fixed allowlist, so catalog content can never open arbitrary schemes.
    pub(crate) fn open_https_url(&self, url: &str) -> AppResult<()> {
        let allowed = [
            "https://github.com/",
            "https://www.npmjs.com/",
            "https://npmjs.com/",
            "https://dsh.market/",
            "https://raw.githubusercontent.com/2BingLing/dsh-market/",
        ];
        if !allowed.iter().any(|prefix| url.starts_with(prefix)) {
            return Err(AppError::new("externalLinkInvalid"));
        }
        self.browsers.open("system", url)
    }
    // -----------------------------------------------------------------------
    // Remote access
    // -----------------------------------------------------------------------

    pub(crate) fn set_remote_master(&self, enabled: bool) -> AppResult<()> {
        self.remote.set_master(enabled)
    }

    pub(crate) fn set_remote_lan_enabled(&self, enabled: bool) -> AppResult<()> {
        self.remote.set_lan_enabled(enabled)
    }

    pub(crate) fn refresh_remote_lan(&self) -> AppResult<()> {
        self.remote.refresh_lan()
    }

    pub(crate) fn set_remote_public_enabled(
        &self,
        enabled: bool,
        acknowledged: bool,
    ) -> AppResult<()> {
        self.remote.set_public_enabled(enabled, acknowledged)
    }

    pub(crate) fn rotate_remote_password(&self, scope: dsh_core::RemoteScope) -> AppResult<()> {
        self.remote.rotate_password(scope)
    }

    pub(crate) fn set_remote_password(
        &self,
        scope: dsh_core::RemoteScope,
        password: String,
    ) -> AppResult<()> {
        self.remote.set_password(scope, password)
    }

    pub(crate) fn retry_remote_public(&self) -> AppResult<()> {
        self.remote.retry_public_tunnel()
    }

    pub(crate) fn remote_qr_svg(&self, scope: dsh_core::RemoteScope) -> AppResult<String> {
        self.remote.qr_svg(scope)
    }

    pub(crate) fn web_url(&self) -> AppResult<String> {
        self.snapshot()
            .web_url
            .ok_or_else(|| AppError::new("serviceNotReady"))
    }
    fn begin_desktop_update(self: &Arc<Self>) -> AppResult<DesktopUpdateOperation> {
        self.desktop_update_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| AppError::new("desktopUpdateBusy"))?;
        Ok(DesktopUpdateOperation {
            state: Arc::clone(self),
        })
    }

    fn begin_harness_update_check(self: &Arc<Self>) -> AppResult<HarnessUpdateCheckOperation> {
        self.harness_update_check_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| AppError::new("harnessUpdateCheckBusy"))?;
        Ok(HarnessUpdateCheckOperation {
            state: Arc::clone(self),
        })
    }

    fn desktop_updater(self: &Arc<Self>) -> AppResult<tauri_plugin_updater::Updater> {
        let state = Arc::clone(self);
        let builder = self.app.updater_builder().on_before_exit(move || {
            // Tauri's Windows updater starts the installer and then calls
            // process::exit(0). Cleanup after Update::install is therefore
            // unreachable on Windows and must live in this hook.
            if let Err(error) = state.prepare_restart() {
                log::error!("service cleanup before updater exit failed: {error:?}");
            }
        });
        // Adapt the unified proxy configuration to the updater. Both the
        // update check and the signed download go through the same
        // `configure_client` hook inside the plugin, so one configuration
        // covers both.
        let builder = match updater_proxy_plan(&network::active())? {
            UpdaterProxyPlan::System => {
                // Explicit takeover only on the Windows path; on macOS/Linux
                // the plugin's default client already merges proxy
                // environment variables with the OS system proxy, and
                // replacing that with an env-only plan would disable the
                // macOS system proxy whenever any single variable is set.
                match network::takeover_system_plan(
                    &|key| std::env::var_os(key),
                    network::platform_proxy_source(),
                ) {
                    Some(plan) => {
                        let proxies = updater_system_proxies(&plan)?;
                        builder.configure_client(move |mut client| {
                            client = client.no_proxy();
                            for proxy in &proxies {
                                client = client.proxy(proxy.clone());
                            }
                            client
                        })
                    }
                    None => builder,
                }
            }
            UpdaterProxyPlan::Direct => builder.no_proxy(),
            UpdaterProxyPlan::Manual { url, bypass } => {
                let proxy = updater_manual_proxy(&url, bypass.as_deref())?;
                builder.configure_client(move |client| client.proxy(proxy.clone()))
            }
        };
        builder.build().map_err(|error| {
            log::error!(
                "desktop updater configuration is invalid: {}",
                updater_error_log(&error)
            );
            AppError::new("desktopUpdateConfigurationInvalid")
        })
    }

    pub(crate) async fn check_desktop_update(
        self: &Arc<Self>,
        report_failure: bool,
    ) -> AppResult<Option<String>> {
        let _operation = self.begin_desktop_update()?;
        self.mutate(|snapshot| snapshot.desktop_update = DesktopUpdateState::Checking);
        let result = async {
            let updater = self.desktop_updater()?;
            tokio::time::timeout(DESKTOP_UPDATE_TIMEOUT, updater.check())
                .await
                .map_err(|_| AppError::new("desktopUpdateCheckTimedOut"))?
                .map_err(|error| {
                    log::warn!(
                        "desktop update check request failed: {}",
                        updater_error_log(&error)
                    );
                    desktop_update_check_error(classify_desktop_update_check_error(&error))
                })
        }
        .await;

        match result {
            Ok(Some(update)) => {
                let version = update.version;
                self.mutate(|snapshot| {
                    snapshot.desktop_update = DesktopUpdateState::Available {
                        version: version.clone(),
                    }
                });
                Ok(Some(version))
            }
            Ok(None) => {
                self.mutate(|snapshot| snapshot.desktop_update = DesktopUpdateState::Idle);
                Ok(None)
            }
            Err(error) => {
                self.mutate(|snapshot| {
                    snapshot.desktop_update = if report_failure {
                        DesktopUpdateState::Failed { version: None }
                    } else {
                        DesktopUpdateState::Idle
                    }
                });
                Err(error)
            }
        }
    }

    pub(crate) async fn install_desktop_update(self: &Arc<Self>) -> AppResult<()> {
        let _operation = self.begin_desktop_update()?;
        let previous_version = match self.snapshot().desktop_update {
            DesktopUpdateState::Available { version }
            | DesktopUpdateState::Failed {
                version: Some(version),
            } => Some(version),
            _ => None,
        };
        self.mutate(|snapshot| {
            snapshot.desktop_update = desktop_update_start_state(previous_version.clone())
        });
        let updater = match self.desktop_updater() {
            Ok(updater) => updater,
            Err(error) => {
                self.mutate(|snapshot| {
                    snapshot.desktop_update = DesktopUpdateState::Failed {
                        version: previous_version,
                    }
                });
                return Err(error);
            }
        };
        let checked = tokio::time::timeout(DESKTOP_UPDATE_TIMEOUT, updater.check()).await;
        let update = match checked {
            Err(_) => {
                self.mutate(|snapshot| {
                    snapshot.desktop_update = DesktopUpdateState::Failed {
                        version: previous_version,
                    }
                });
                return Err(AppError::new("desktopUpdateCheckTimedOut"));
            }
            Ok(Ok(Some(update))) => update,
            Ok(Ok(None)) => {
                self.mutate(|snapshot| snapshot.desktop_update = DesktopUpdateState::Idle);
                return Err(AppError::new("desktopUpdateNotAvailable"));
            }
            Ok(Err(error)) => {
                log::warn!(
                    "desktop update check before download failed: {}",
                    updater_error_log(&error)
                );
                self.mutate(|snapshot| {
                    snapshot.desktop_update = DesktopUpdateState::Failed {
                        version: previous_version,
                    }
                });
                return Err(desktop_update_check_error(
                    classify_desktop_update_check_error(&error),
                ));
            }
        };

        // Always use the version returned by this fresh check. If a newer
        // release appeared after the prompt, it replaces the stale version
        // instead of trapping the user in a permanent mismatch loop.
        let version = update.version.clone();
        let mut attempt = 1;
        let bytes = loop {
            self.mutate(|snapshot| {
                snapshot.desktop_update = DesktopUpdateState::Downloading {
                    version: version.clone(),
                    done: 0,
                    total: None,
                }
            });
            let progress_version = version.clone();
            let weak = Arc::downgrade(self);
            let progress = Arc::new(Mutex::new(DownloadProgressThrottle::default()));
            let downloaded = tokio::time::timeout(
                DESKTOP_UPDATE_DOWNLOAD_TIMEOUT,
                update.download(
                    move |chunk, total| {
                        if let Some(state) = weak.upgrade() {
                            let done = progress
                                .lock()
                                .expect("desktop update progress poisoned")
                                .record(chunk, total, Instant::now());
                            if let Some(done) = done {
                                state.mutate(|snapshot| {
                                    snapshot.desktop_update = DesktopUpdateState::Downloading {
                                        version: progress_version.clone(),
                                        done,
                                        total,
                                    };
                                });
                            }
                        }
                    },
                    || {},
                ),
            )
            .await;
            match downloaded {
                Ok(Ok(bytes)) => break bytes,
                Ok(Err(error)) => {
                    let failure = classify_desktop_update_download_error(&error);
                    if should_retry_desktop_update_download(attempt, failure) {
                        log::warn!(
                            "desktop update download attempt {attempt}/{DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS} failed; will retry: {}",
                            updater_error_log(&error)
                        );
                        self.mutate(|snapshot| {
                            snapshot.desktop_update = DesktopUpdateState::Downloading {
                                version: version.clone(),
                                done: 0,
                                total: None,
                            }
                        });
                        attempt += 1;
                        tokio::time::sleep(DESKTOP_UPDATE_RETRY_DELAY).await;
                        continue;
                    }
                    log::warn!(
                        "desktop update download attempt {attempt}/{DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS} failed; giving up: {}",
                        updater_error_log(&error)
                    );
                    self.mutate(|snapshot| {
                        snapshot.desktop_update = DesktopUpdateState::Failed {
                            version: Some(version.clone()),
                        }
                    });
                    return Err(desktop_update_download_error(failure));
                }
                Err(_) => {
                    // One attempt may already have occupied the full 30-minute
                    // budget, so a timeout is left for an explicit user retry.
                    log::warn!(
                        "desktop update download attempt {attempt}/{DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS} timed out; giving up"
                    );
                    self.mutate(|snapshot| {
                        snapshot.desktop_update = DesktopUpdateState::Failed {
                            version: Some(version.clone()),
                        }
                    });
                    return Err(AppError::new("desktopUpdateDownloadTimedOut"));
                }
            }
        };

        self.mutate(|snapshot| {
            snapshot.desktop_update = DesktopUpdateState::Installing {
                version: version.clone(),
            }
        });
        if let Err(error) = update.install(bytes) {
            log::warn!(
                "desktop update install failed: {}",
                updater_error_log(&error)
            );
            self.mutate(|snapshot| {
                snapshot.desktop_update = DesktopUpdateState::Failed {
                    version: Some(version),
                }
            });
            return Err(AppError::new("desktopUpdateFailed"));
        }

        // On Windows Update::install exits the process after invoking the
        // on_before_exit hook, so this branch is reached only on macOS/Linux.
        self.prepare_restart()?;
        self.app.restart();
    }
    pub(crate) fn prepare_restart(self: &Arc<Self>) -> AppResult<()> {
        self.quitting.store(true, Ordering::SeqCst);
        let deployments = self.cancel_deployments();
        let stopped = self.complete_process_cleanup(deployments);
        match stopped {
            Ok(()) => {
                self.hide_tray_before_exit();
                self.exit_ready.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => {
                self.quitting.store(false, Ordering::SeqCst);
                Err(error)
            }
        }
    }
    pub(crate) fn quit(self: &Arc<Self>) {
        if self.quitting.swap(true, Ordering::SeqCst) {
            return;
        }
        let deployments = self.cancel_deployments();
        self.mutate(|snapshot| snapshot.phase = LauncherPhase::Stopping);
        let state = Arc::clone(self);
        if let Err(error) = thread::Builder::new()
            .name("launcher-shutdown".into())
            .spawn(move || match state.complete_process_cleanup(deployments) {
                Ok(()) => state.exit_after_cleanup(),
                Err(error) => state.shutdown_failed(error),
            })
        {
            log::error!("shutdown worker could not start: {error}");
            let deployments = self.cancel_deployments();
            match self.complete_process_cleanup(deployments) {
                Ok(()) => self.exit_after_cleanup(),
                Err(error) => self.shutdown_failed(error),
            }
        }
    }

    fn shutdown_failed(&self, error: AppError) {
        log::error!("service cleanup failed; application exit was cancelled: {error:?}");
        self.exit_ready.store(false, Ordering::SeqCst);
        self.quitting.store(false, Ordering::SeqCst);
        self.fail(error);
        show_main_window(&self.app);
    }

    fn exit_after_cleanup(&self) {
        self.hide_tray_before_exit();
        self.exit_ready.store(true, Ordering::SeqCst);
        self.app.exit(0);
    }

    fn hide_tray_before_exit(&self) {
        if let Some(tray) = self.tray.lock().expect("tray poisoned").as_ref()
            && let Err(error) = tray.set_visible(false)
        {
            log::warn!("system tray icon could not be removed before exit: {error}");
        }
    }

    fn cancel_deployments(&self) -> Vec<DeploymentController> {
        let foreground = self
            .deployment
            .lock()
            .expect("deployment poisoned")
            .as_ref()
            .cloned();
        let background = self
            .background_update
            .lock()
            .expect("background update poisoned")
            .as_ref()
            .cloned();
        let controllers = [foreground, background]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        for controller in &controllers {
            controller.cancel();
        }
        controllers
    }

    fn complete_process_cleanup(&self, deployments: Vec<DeploymentController>) -> AppResult<()> {
        self.join_startup();
        self.join_background_update();
        let deployment_error = deployments
            .into_iter()
            .find_map(|controller| controller.cleanup_error());
        self.server.lock().expect("server poisoned").stop()?;
        self.remote.shutdown();
        if let Some(error) = deployment_error {
            return Err(error);
        }
        Ok(())
    }

    fn join_startup(&self) {
        let worker = self
            .startup_thread
            .lock()
            .expect("startup thread poisoned")
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    fn join_background_update(&self) {
        let worker = self
            .background_update_thread
            .lock()
            .expect("background update thread poisoned")
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    fn refresh_tray_menu(&self, language: Language) -> tauri::Result<()> {
        let menu = tray_menu(&self.app, language, self.snapshot().pet.enabled)?;
        if let Some(tray) = self.tray.lock().expect("tray poisoned").as_ref() {
            tray.set_menu(Some(menu))?;
        }
        Ok(())
    }
}

fn update_market_operation_state(snapshot: &mut LauncherSnapshot, busy: bool) -> bool {
    if snapshot.market_busy == busy {
        return false;
    }
    snapshot.market_busy = busy;
    if !busy {
        snapshot.market_revision = snapshot.market_revision.saturating_add(1);
    }
    true
}

fn mark_harness_update_checking(
    snapshot: &mut LauncherSnapshot,
    expected: &HarnessUpdateState,
) -> bool {
    if !matches!(
        snapshot.phase,
        LauncherPhase::Ready | LauncherPhase::Stopped
    ) || &snapshot.harness_update != expected
    {
        return false;
    }
    snapshot.harness_update = HarnessUpdateState::Checking;
    true
}

fn complete_harness_deployment(snapshot: &mut LauncherSnapshot, version: String, forced: bool) {
    snapshot.harness_version = Some(version);
    if forced {
        snapshot.harness_update = HarnessUpdateState::None;
    }
}

fn fail_start_worker_spawn(
    snapshot: &mut LauncherSnapshot,
    previous_update: HarnessUpdateState,
    failure: AppError,
) {
    snapshot.phase = LauncherPhase::Failed;
    snapshot.activity = None;
    snapshot.harness_update = previous_update;
    snapshot.error = Some(failure);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopUpdateCheckFailure {
    Network,
    Other,
}

fn classify_desktop_update_check_error(
    error: &tauri_plugin_updater::Error,
) -> DesktopUpdateCheckFailure {
    match error {
        tauri_plugin_updater::Error::Reqwest(_) => DesktopUpdateCheckFailure::Network,
        _ => DesktopUpdateCheckFailure::Other,
    }
}

fn desktop_update_check_error(failure: DesktopUpdateCheckFailure) -> AppError {
    let code = match failure {
        DesktopUpdateCheckFailure::Network => "desktopUpdateCheckNetworkFailed",
        DesktopUpdateCheckFailure::Other => "desktopUpdateCheckFailed",
    };
    AppError::new(code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopUpdateDownloadFailure {
    RetryableNetwork,
    Trust,
    Other,
}

fn classify_desktop_update_download_error(
    error: &tauri_plugin_updater::Error,
) -> DesktopUpdateDownloadFailure {
    use tauri_plugin_updater::Error;

    match error {
        Error::Reqwest(_) => DesktopUpdateDownloadFailure::RetryableNetwork,
        Error::Network(message) if retryable_download_http_status(message) => {
            DesktopUpdateDownloadFailure::RetryableNetwork
        }
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            DesktopUpdateDownloadFailure::Trust
        }
        _ => DesktopUpdateDownloadFailure::Other,
    }
}

fn retryable_download_http_status(message: &str) -> bool {
    let status = message
        .strip_prefix("Download request failed with status:")
        .and_then(|suffix| suffix.split_whitespace().next())
        .and_then(|status| status.parse::<u16>().ok());

    matches!(status, Some(408 | 429 | 500..=599))
}

fn should_retry_desktop_update_download(
    attempt: usize,
    failure: DesktopUpdateDownloadFailure,
) -> bool {
    failure == DesktopUpdateDownloadFailure::RetryableNetwork
        && attempt < DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS
}

fn desktop_update_download_error(failure: DesktopUpdateDownloadFailure) -> AppError {
    let code = match failure {
        DesktopUpdateDownloadFailure::RetryableNetwork => "desktopUpdateNetworkFailed",
        DesktopUpdateDownloadFailure::Trust => "desktopUpdateTrustInvalid",
        DesktopUpdateDownloadFailure::Other => "desktopUpdateDownloadFailed",
    };
    AppError::new(code)
}

fn desktop_update_start_state(version: Option<String>) -> DesktopUpdateState {
    version.map_or(DesktopUpdateState::Checking, |version| {
        DesktopUpdateState::Preparing { version }
    })
}

fn replace_harness_update_if_checking(
    snapshot: &mut LauncherSnapshot,
    replacement: HarnessUpdateState,
) -> bool {
    if snapshot.harness_update == HarnessUpdateState::Checking {
        snapshot.harness_update = replacement;
        true
    } else {
        false
    }
}

fn harness_update_after_check(
    previous: HarnessUpdateState,
    current: &Version,
    latest: &Version,
) -> HarnessUpdateState {
    let known_version = match &previous {
        HarnessUpdateState::Available { version }
        | HarnessUpdateState::Downloaded { version }
        | HarnessUpdateState::Failed { version } => Version::parse(version).ok(),
        _ => None,
    };
    if known_version.as_ref().is_some_and(|known| known >= latest) {
        return previous;
    }
    if latest > current {
        HarnessUpdateState::Available {
            version: latest.to_string(),
        }
    } else {
        HarnessUpdateState::None
    }
}

fn acquire_instance_lock(paths: &ApplicationPaths) -> AppResult<File> {
    acquire_instance_lock_with_timeout(paths, INSTANCE_LOCK_TIMEOUT)
}

fn acquire_instance_lock_with_timeout(
    paths: &ApplicationPaths,
    timeout: Duration,
) -> AppResult<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.launcher_lock)
        .map_err(|error| AppError::io("launcherLockFailed", &error))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == lock_contended_error().kind() => {
                if std::time::Instant::now() >= deadline {
                    return Err(AppError::new("launcherAlreadyRunning"));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(AppError::io("launcherLockFailed", &error)),
        }
    }
}

struct DesktopUpdateOperation {
    state: Arc<AppState>,
}

struct HarnessUpdateCheckOperation {
    state: Arc<AppState>,
}

impl Drop for HarnessUpdateCheckOperation {
    fn drop(&mut self) {
        self.state
            .harness_update_check_busy
            .store(false, Ordering::SeqCst);
    }
}

impl Drop for DesktopUpdateOperation {
    fn drop(&mut self) {
        self.state
            .desktop_update_busy
            .store(false, Ordering::SeqCst);
    }
}

fn tray_menu(
    app: &AppHandle,
    language: Language,
    pet_enabled: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let (show, open_web, pet, quit) = match (language, pet_enabled) {
        (Language::Zh, true) => (
            "打开启动主页面",
            "打开 DeepSeek Harness 工作台",
            "隐藏桌面宠物",
            "退出",
        ),
        (Language::Zh, false) => (
            "打开启动主页面",
            "打开 DeepSeek Harness 工作台",
            "显示桌面宠物",
            "退出",
        ),
        (Language::En, true) => (
            "Open launcher",
            "Open DeepSeek Harness Workspace",
            "Hide desktop pet",
            "Quit",
        ),
        (Language::En, false) => (
            "Open launcher",
            "Open DeepSeek Harness Workspace",
            "Show desktop pet",
            "Quit",
        ),
    };
    let show = MenuItem::with_id(app, "open", show, true, None::<&str>)?;
    let open_web = MenuItem::with_id(app, "web", open_web, true, None::<&str>)?;
    let pet = MenuItem::with_id(app, "pet", pet, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &open_web, &pet, &quit])
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.show() {
            log::warn!("main window could not be shown: {error}");
            return;
        }
        if let Err(error) = window.unminimize() {
            log::warn!("main window could not be unminimized: {error}");
        }
        if let Err(error) = window.set_focus() {
            log::warn!("main window could not be focused: {error}");
        }
    }
}

fn activate_main_window(app: &AppHandle) {
    if app.get_webview_window("main").is_some() {
        show_main_window(app);
    } else if let Some(activation) = app.try_state::<MainWindowActivation>() {
        activation.request();
    }
}

fn report_startup_failure(app: &AppHandle, error: &str) {
    log::error!("launcher setup failed before the main window became available: {error}");
    app.dialog()
        .message(format!(
            "DSH Launcher 无法启动。\n\nDSH Launcher could not start.\n\n{error}"
        ))
        .title("DSH Launcher")
        .kind(MessageDialogKind::Error)
        .blocking_show();
}

fn install_tray(app: &tauri::App, state: &Arc<AppState>) -> tauri::Result<()> {
    let current = state.snapshot();
    let menu = tray_menu(app.handle(), current.language, current.pet.enabled)?;
    let weak_menu = Arc::downgrade(state);
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("DSH Launcher")
        .menu(&menu)
        .on_tray_icon_event(move |tray, event| {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(move |app, event| {
            if let Some(state) = weak_menu.upgrade() {
                match event.id().as_ref() {
                    "open" => show_main_window(app),
                    "web" => {
                        let _ = state.open_web_ui();
                    }
                    "pet" => {
                        let enabled = !state.snapshot().pet.enabled;
                        if let Err(error) = state.patch_pet_preferences(PetPreferencesPatch {
                            enabled: Some(enabled),
                            ..PetPreferencesPatch::default()
                        }) {
                            log::warn!("desktop pet toggle failed: {error}");
                        }
                    }
                    "quit" => state.quit(),
                    _ => {}
                }
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    } else {
        // A missing bundle icon must not panic the Tauri setup thread. Some
        // development or damaged installations may not expose one; building
        // without it lets the platform return an ordinary, recoverable tray
        // error while the main window and Harness startup continue.
        log::warn!("bundle icon unavailable; attempting tray creation without an icon");
    }
    let tray = builder.build(app)?;
    *state.tray.lock().expect("tray poisoned") = Some(tray);
    state.mutate(|snapshot| snapshot.tray_available = true);
    Ok(())
}

async fn check_desktop_update_after_startup(state: Arc<AppState>) {
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = state.check_desktop_update(false).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleDecision {
    KeepRunning,
    QuitAfterCleanup,
    AllowExit,
}

fn lifecycle_decision(exit_ready: bool, tray_available: bool) -> LifecycleDecision {
    if exit_ready {
        LifecycleDecision::AllowExit
    } else if tray_available {
        LifecycleDecision::KeepRunning
    } else {
        LifecycleDecision::QuitAfterCleanup
    }
}

fn setup_application(app: &mut tauri::App) -> Result<(), String> {
    let main_window_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .cloned()
        .ok_or_else(|| "main window configuration is missing".to_owned())?;
    let pet_window_config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "pet")
        .cloned();
    let paths = ApplicationPaths::from_environment()
        .map_err(|error| format!("application paths are unavailable: {error}"))?;
    paths
        .ensure_dirs()
        .map_err(|error| format!("application directories are unavailable: {error}"))?;
    let state = AppState::new(app.handle().clone(), paths)
        .map_err(|error| format!("application state could not be initialized: {error}"))?;
    app.manage(Arc::clone(&state));
    // Configured windows are otherwise created before this setup hook. Build them only
    // after AppState is managed so a fast WebView2 page cannot invoke a stateful command
    // while the second (pet) webview is still being constructed.
    tauri::WebviewWindowBuilder::from_config(app.handle(), &main_window_config)
        .map_err(|error| format!("main window could not be configured: {error}"))?
        .build()
        .map_err(|error| format!("main window could not be created: {error}"))?;
    if app.state::<MainWindowActivation>().take_pending() {
        show_main_window(app.handle());
    }
    if let Some(config) = pet_window_config {
        let pet_window = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)
            .and_then(|builder| builder.build());
        if let Err(error) = pet_window {
            log::warn!("desktop pet window unavailable; continuing without it: {error}");
        }
    }
    if let Err(error) = install_tray(app, &state) {
        log::warn!("system tray unavailable; closing the window will exit: {error}");
    }
    if let Err(error) = state.start(false, None) {
        // The desktop shell is the recovery surface for deployment
        // and service failures. Worker-spawn failures already record
        // a Failed snapshot; preserve that state and adapt any other
        // synchronous failure before continuing Tauri setup.
        if state.snapshot().phase != LauncherPhase::Failed {
            state.fail(error.clone());
        }
        log::error!("automatic Harness startup could not begin: {error:?}");
    }
    tauri::async_runtime::spawn(check_desktop_update_after_startup(state));
    Ok(())
}

pub fn run() {
    if dsh_core::service::handle_service_guard_cli() || handle_cli_probe() {
        return;
    }
    tauri::Builder::default()
        .manage(MainWindowActivation::default())
        // This must remain the first registered plugin: later plugins and setup work should
        // only run in the primary process. The file lock in AppState remains a final safeguard.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            activate_main_window(app);
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            setup_application(app).map_err(|error| {
                report_startup_failure(app.handle(), &error);
                error.into()
            })
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && let Some(state) = window.app_handle().try_state::<Arc<AppState>>()
            {
                if window.label() == "pet" && !state.exit_ready.load(Ordering::SeqCst) {
                    api.prevent_close();
                    if let Err(error) = window.hide() {
                        log::warn!("pet window could not be hidden: {error}");
                    }
                    return;
                }
                match lifecycle_decision(
                    state.exit_ready.load(Ordering::SeqCst),
                    state.snapshot().tray_available,
                ) {
                    LifecycleDecision::KeepRunning => {
                        api.prevent_close();
                        if let Err(error) = window.hide() {
                            log::warn!("main window could not be hidden: {error}");
                        }
                    }
                    LifecycleDecision::QuitAfterCleanup => {
                        api.prevent_close();
                        state.quit();
                    }
                    LifecycleDecision::AllowExit => {}
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::launcher_get_snapshot,
            commands::launcher_retry,
            commands::launcher_rollback_harness,
            commands::launcher_repair_and_start,
            commands::launcher_acknowledge_startup_repair,
            commands::launcher_startup_repair_backups,
            commands::launcher_clear_startup_repair_backups,
            commands::launcher_stop,
            commands::launcher_restart,
            commands::launcher_check_harness_update,
            commands::launcher_update_harness,
            commands::launcher_activate_harness_update,
            commands::migration_approve,
            commands::migration_skip,
            commands::launcher_select_browser,
            commands::launcher_open_web_ui,
            commands::application_open_website,
            commands::application_open_external_link,
            commands::application_copy_web_url,
            commands::preferences_set_language,
            commands::preferences_set_theme,
            commands::preferences_set_show_balance_card,
            commands::preferences_patch_pet,
            commands::pet_get_snapshot,
            commands::preferences_set_harness_update_channel,
            commands::preferences_set_proxy,
            commands::proxy_test_connection,
            commands::balance_get_snapshot,
            commands::balance_refresh,
            commands::application_check_update,
            commands::application_install_update,
            commands::market_get_catalog,
            commands::market_refresh_catalog,
            commands::market_refresh_if_stale,
            commands::market_query,
            commands::market_installed,
            commands::market_compatibility,
            commands::market_compatibility_batch,
            commands::market_inspect,
            commands::market_install,
            commands::market_uninstall,
            commands::market_pending_verification,
            commands::market_rollback_pending,
            commands::market_open_plugin_github,
            commands::remote_set_master,
            commands::remote_set_lan_enabled,
            commands::remote_refresh_lan,
            commands::remote_set_public_enabled,
            commands::remote_rotate_password,
            commands::remote_set_password,
            commands::remote_retry_public,
            commands::remote_qr,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DSH Launcher")
        .run(|app, event| match event {
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => {
                // Dock activation always targets the launcher. A visible pet or an
                // unfocused main window must not suppress restoring the main window.
                activate_main_window(app);
            }
            RunEvent::ExitRequested { api, .. } => {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    match lifecycle_decision(
                        state.exit_ready.load(Ordering::SeqCst),
                        state.snapshot().tray_available,
                    ) {
                        LifecycleDecision::KeepRunning => api.prevent_exit(),
                        LifecycleDecision::QuitAfterCleanup => {
                            api.prevent_exit();
                            state.quit();
                        }
                        LifecycleDecision::AllowExit => {}
                    }
                }
            }
            _ => {}
        });
}

fn handle_cli_probe() -> bool {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.iter().any(|value| value == "--desktop-version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return true;
    }
    if arguments.iter().any(|value| value == "--check") {
        let result = ApplicationPaths::from_environment().and_then(|paths| {
            if std::env::var_os("DSH_DESKTOP_HOME").is_none() {
                return Err(AppError::new("checkRequiresIsolatedHome"));
            }
            paths.ensure_dirs()
        });
        match result {
            Ok(()) => println!("DSH Launcher check passed"),
            Err(error) => {
                eprintln!("DSH Launcher check failed: {error}");
                std::process::exit(1);
            }
        }
        return true;
    }
    false
}

struct MarketActivationService<'a> {
    app: &'a AppState,
    defer_restart: bool,
}

impl MarketService for MarketActivationService<'_> {
    fn defer_restart(&mut self) -> bool {
        self.defer_restart
            || !self
                .app
                .server
                .lock()
                .expect("server poisoned")
                .can_restart_automatically()
    }

    fn stop(&mut self) -> AppResult<()> {
        if let Err(error) = self.app.server.lock().expect("server poisoned").stop() {
            self.app.fail(error.clone());
            return Err(error);
        }
        self.app.mutate(|snapshot| {
            snapshot.phase = LauncherPhase::Starting;
            snapshot.web_url = None;
            snapshot.service_started_at_ms = None;
            snapshot.activity = Some(ActivityState {
                code: ActivityCode::StartingService,
                values: Default::default(),
                started_at_ms: now_ms(),
            });
            snapshot.error = None;
        });
        Ok(())
    }

    fn start(&mut self) -> AppResult<()> {
        let outcome = {
            let mut server = self.app.server.lock().expect("server poisoned");
            server
                .start_cancellable(|| self.app.quitting.load(Ordering::SeqCst))
                .and_then(|url| {
                    server.verify_web_ready(&url, || self.app.quitting.load(Ordering::SeqCst))?;
                    Ok(url)
                })
        };
        match outcome {
            Ok(url) => {
                self.app.mutate(|snapshot| {
                    snapshot.phase = LauncherPhase::Ready;
                    snapshot.web_url = Some(url);
                    snapshot.service_started_at_ms = Some(now_ms());
                    snapshot.activity = None;
                    snapshot.error = None;
                });
                Ok(())
            }
            Err(error) => {
                self.app.fail(error.clone());
                Err(error)
            }
        }
    }
}

/// A marketplace rollback is justified only when the managed process ran but
/// never published its Web UI address. Infrastructure failures (port
/// allocation, guard spawn/ownership, output capture, shutdown) leave the
/// unverified batch intact for diagnosis instead of blaming and deleting an
/// unrelated plugin change. Runtime replacement is also excluded because the
/// new runtime itself is an independent cause.
fn should_rollback_marketplace_after_start_failure(
    error: &AppError,
    runtime_replaced: bool,
) -> bool {
    !runtime_replaced && error.code == "serviceNoAddress"
}

fn incompatible_plugin_packages(error: &AppError) -> Vec<String> {
    if error.code != "serviceNoAddress" {
        return Vec::new();
    }
    let mut packages = error
        .values
        .get("incompatiblePlugins")
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    packages.sort_unstable();
    packages.dedup();
    packages
}

fn retain_incompatible_plugin_context(mut error: AppError, packages: &[String]) -> AppError {
    if !packages.is_empty() {
        error
            .values
            .insert("incompatiblePlugins".into(), packages.join(","));
    }
    error
}

fn clear_startup_repair_notice(snapshot: &mut LauncherSnapshot) -> bool {
    if snapshot.removed_incompatible_plugins.is_empty() && !snapshot.repaired_projection_cache {
        return false;
    }
    snapshot.removed_incompatible_plugins.clear();
    snapshot.repaired_projection_cache = false;
    true
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use super::{
        DEEPSEEK_PLATFORM, DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS, DesktopUpdateCheckFailure,
        DesktopUpdateDownloadFailure, DownloadProgressThrottle, GITHUB_REPOSITORY,
        HARNESS_GITHUB_REPOSITORY, LifecycleDecision, MainWindowActivation,
        PROGRESS_EVENT_INTERVAL, ProgressEventThrottle, UpdaterProxyPlan, WEBSITE,
        acquire_instance_lock, acquire_instance_lock_with_timeout, apply_updater_system_plan,
        classify_desktop_update_check_error, classify_desktop_update_download_error,
        clear_startup_repair_notice, complete_harness_deployment, desktop_update_check_error,
        desktop_update_download_error, desktop_update_start_state, external_link_url,
        fail_start_worker_spawn, harness_update_after_check, incompatible_plugin_packages,
        lifecycle_decision, mark_harness_update_checking, replace_harness_update_if_checking,
        retain_incompatible_plugin_context, retryable_download_http_status,
        should_retry_desktop_update_download, should_rollback_marketplace_after_start_failure,
        update_market_operation_state, updater_error_log, updater_manual_proxy, updater_proxy_plan,
    };
    use dsh_core::{
        AppError, ApplicationPaths, DesktopUpdateState, HarnessUpdateState, LauncherPhase,
        LauncherSnapshot, ProxyMode, ProxySettings,
    };
    use semver::Version;

    #[test]
    fn main_window_activation_remembers_early_second_launch() {
        let activation = MainWindowActivation::default();
        assert!(!activation.take_pending());

        activation.request();
        assert!(activation.take_pending());
        assert!(!activation.take_pending());
    }

    #[test]
    fn configured_webviews_wait_for_application_setup() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows = config["app"]["windows"].as_array().unwrap();
        for label in ["main", "pet"] {
            let window = windows
                .iter()
                .find(|window| window["label"] == label)
                .unwrap();
            assert_eq!(window["create"], false, "{label} loaded before setup");
        }
    }

    #[test]
    fn updater_proxy_plan_maps_each_mode() {
        assert_eq!(
            updater_proxy_plan(&ProxySettings::default()).unwrap(),
            UpdaterProxyPlan::System
        );
        let direct = ProxySettings {
            mode: ProxyMode::Direct,
            ..ProxySettings::default()
        };
        assert_eq!(
            updater_proxy_plan(&direct).unwrap(),
            UpdaterProxyPlan::Direct
        );
        let manual = ProxySettings {
            mode: ProxyMode::Manual,
            url: "http://127.0.0.1:7890".into(),
            bypass: "localhost; *.internal".into(),
        };
        assert_eq!(
            updater_proxy_plan(&manual).unwrap(),
            UpdaterProxyPlan::Manual {
                url: url::Url::parse("http://127.0.0.1:7890").unwrap(),
                // The bypass list survives onto the updater path, normalized
                // to reqwest's domain grammar.
                bypass: Some("localhost,.internal".to_owned()),
            }
        );
    }

    #[test]
    fn updater_proxy_plan_supports_socks_and_builds_proxies_with_bypass() {
        for url in ["socks5://127.0.0.1:1080", "socks5h://127.0.0.1:1080"] {
            let manual = ProxySettings {
                mode: ProxyMode::Manual,
                url: url.into(),
                bypass: String::new(),
            };
            let plan = updater_proxy_plan(&manual).unwrap();
            let UpdaterProxyPlan::Manual {
                url: parsed,
                bypass,
            } = plan
            else {
                panic!("expected manual plan");
            };
            assert_eq!(parsed.as_str().trim_end_matches('/'), url);
            assert_eq!(bypass, None);
            // The reqwest 0.13 alias must build proxies for SOCKS URLs (the
            // socks feature is unified onto the updater plugin's reqwest).
            let _ = updater_manual_proxy(&parsed, None).unwrap();
        }
        let url = url::Url::parse("http://127.0.0.1:7890").unwrap();
        let _ = updater_manual_proxy(&url, Some("localhost,.internal")).unwrap();
        // System plan application is a no-op for an empty plan and succeeds
        // for a populated one.
        let empty = dsh_core::network::SystemProxyPlan::default();
        let _ = apply_updater_system_plan(reqwest_updater::ClientBuilder::new(), &empty).unwrap();
        let plan = dsh_core::network::SystemProxyPlan {
            http: Some("http://127.0.0.1:7890".into()),
            https: Some("socks5://127.0.0.1:1080".into()),
            all: None,
            no_proxy: Some("localhost".into()),
        };
        let _ = apply_updater_system_plan(reqwest_updater::ClientBuilder::new(), &plan).unwrap();
    }

    #[test]
    fn updater_proxy_plan_rejects_invalid_manual_urls() {
        for url in [
            "",
            "not-a-url",
            "ftp://127.0.0.1:21",
            "http://user:pw@127.0.0.1:1",
            "http://127.0.0.1:8080/path",
            "http://127.0.0.1:8080?x=1",
        ] {
            let manual = ProxySettings {
                mode: ProxyMode::Manual,
                url: url.into(),
                bypass: String::new(),
            };
            let error = updater_proxy_plan(&manual).expect_err(url);
            assert_eq!(error.code, "proxyUrlInvalid", "{url}");
            assert_eq!(error.safe_detail, None, "{url}");
            assert!(!format!("{error:?}").contains("user:pw"), "{url}");
        }
    }

    #[test]
    fn updater_error_logging_never_contains_proxy_credentials() {
        let with_creds = tauri_plugin_updater::Error::Network(
            "connect via http://user:topsecret@proxy.local:3128 failed".to_owned(),
        );
        let rendered = updater_error_log(&with_creds);
        assert!(
            rendered.contains("http://***@proxy.local:3128"),
            "{rendered}"
        );
        assert!(!rendered.contains("topsecret"), "{rendered}");
        assert!(!rendered.contains("user@"), "{rendered}");
        let deterministic = tauri_plugin_updater::Error::EmptyEndpoints;
        assert_eq!(updater_error_log(&deterministic), deterministic.to_string());
    }

    #[test]
    fn product_website_uses_the_public_homepage() {
        assert_eq!(WEBSITE, "https://dsdesktop.com/");
    }

    #[test]
    fn market_operation_state_publishes_busy_and_one_completion_revision() {
        let mut snapshot = LauncherSnapshot::initial("0.3.4");
        assert!(update_market_operation_state(&mut snapshot, true));
        assert!(snapshot.market_busy);
        assert_eq!(snapshot.market_revision, 0);
        assert!(!update_market_operation_state(&mut snapshot, true));

        assert!(update_market_operation_state(&mut snapshot, false));
        assert!(!snapshot.market_busy);
        assert_eq!(snapshot.market_revision, 1);
        assert!(!update_market_operation_state(&mut snapshot, false));
        assert_eq!(snapshot.market_revision, 1);
    }

    #[test]
    fn startup_worker_spawn_failure_keeps_the_shell_recoverable() {
        let mut snapshot = LauncherSnapshot::initial("test");
        let downloaded = HarnessUpdateState::Downloaded {
            version: "1.2.3".into(),
        };
        snapshot.harness_update = HarnessUpdateState::Installing {
            version: "1.2.3".into(),
        };
        let failure = AppError::new("launcherWorkerFailed");

        fail_start_worker_spawn(&mut snapshot, downloaded.clone(), failure.clone());

        assert_eq!(snapshot.phase, LauncherPhase::Failed);
        assert_eq!(snapshot.harness_update, downloaded);
        assert_eq!(snapshot.error, Some(failure));
        assert!(snapshot.activity.is_none());
    }

    #[test]
    fn progress_events_are_throttled_but_completion_is_immediate() {
        let started = Instant::now();
        let mut throttle = ProgressEventThrottle::default();
        assert!(throttle.should_emit(10, Some(100), started));
        assert!(!throttle.should_emit(20, Some(100), started + PROGRESS_EVENT_INTERVAL / 2));
        assert!(throttle.should_emit(30, Some(100), started + PROGRESS_EVENT_INTERVAL));
        assert!(throttle.should_emit(
            100,
            Some(100),
            started + PROGRESS_EVENT_INTERVAL + Duration::from_millis(1)
        ));
    }

    #[test]
    fn coalesced_download_progress_never_loses_bytes() {
        let started = Instant::now();
        let mut progress = DownloadProgressThrottle::default();
        assert_eq!(progress.record(10, Some(100), started), Some(10));
        assert_eq!(
            progress.record(20, Some(100), started + PROGRESS_EVENT_INTERVAL / 2),
            None
        );
        assert_eq!(
            progress.record(70, Some(100), started + PROGRESS_EVENT_INTERVAL / 2),
            Some(100)
        );
    }

    #[test]
    fn external_links_are_limited_to_known_destinations() {
        assert_eq!(external_link_url("github"), Some(GITHUB_REPOSITORY));
        assert_eq!(
            external_link_url("harnessGithub"),
            Some(HARNESS_GITHUB_REPOSITORY)
        );
        assert_eq!(external_link_url("deepseek"), Some(DEEPSEEK_PLATFORM));
        assert_eq!(external_link_url("unknown"), None);
    }

    #[test]
    fn marketplace_rollback_is_limited_to_unverified_service_boot_failures() {
        assert!(should_rollback_marketplace_after_start_failure(
            &AppError::new("serviceNoAddress"),
            false
        ));
        for unrelated in [
            "freePortFailed",
            "serviceStartFailed",
            "serviceGuardFailed",
            "serviceOutputUnreadable",
            "serviceShutdownIncomplete",
        ] {
            assert!(!should_rollback_marketplace_after_start_failure(
                &AppError::new(unrelated),
                false
            ));
        }
        assert!(!should_rollback_marketplace_after_start_failure(
            &AppError::new("serviceNoAddress"),
            true
        ));
    }

    #[test]
    fn incompatible_plugin_recovery_uses_only_sanitized_service_values() {
        let error = AppError::new("serviceNoAddress")
            .value("incompatiblePlugins", "dsh-zeta,@scope/alpha,dsh-zeta");
        assert_eq!(
            incompatible_plugin_packages(&error),
            ["@scope/alpha", "dsh-zeta"]
        );
        assert!(incompatible_plugin_packages(&AppError::new("freePortFailed")).is_empty());
    }

    #[test]
    fn failed_plugin_retry_retains_sanitized_candidates_for_combined_repair() {
        let error = AppError::new("serviceNoAddress").value("repairableProjectionCache", true);
        let packages = vec!["dsh-better-sidebar".into()];

        let retained = retain_incompatible_plugin_context(error, &packages);

        assert_eq!(
            retained
                .values
                .get("incompatiblePlugins")
                .map(String::as_str),
            Some("dsh-better-sidebar")
        );
        assert_eq!(
            retained
                .values
                .get("repairableProjectionCache")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn acknowledging_startup_repair_consumes_the_snapshot_notice() {
        let mut snapshot = LauncherSnapshot {
            removed_incompatible_plugins: vec!["dsh-better-sidebar".into()],
            repaired_projection_cache: true,
            ..LauncherSnapshot::initial("test")
        };

        assert!(clear_startup_repair_notice(&mut snapshot));
        assert!(snapshot.removed_incompatible_plugins.is_empty());
        assert!(!snapshot.repaired_projection_cache);
        assert!(!clear_startup_repair_notice(&mut snapshot));
    }

    #[test]
    fn installing_a_known_desktop_update_preserves_its_target_version() {
        assert_eq!(
            desktop_update_start_state(Some("0.3.1".into())),
            DesktopUpdateState::Preparing {
                version: "0.3.1".into()
            }
        );
    }

    #[test]
    fn direct_desktop_install_requests_fall_back_to_checking() {
        assert_eq!(
            desktop_update_start_state(None),
            DesktopUpdateState::Checking
        );
    }

    #[test]
    fn desktop_update_check_errors_distinguish_network_and_other_failures() {
        let deterministic = tauri_plugin_updater::Error::EmptyEndpoints;
        assert_eq!(
            classify_desktop_update_check_error(&deterministic),
            DesktopUpdateCheckFailure::Other
        );
        assert_eq!(
            desktop_update_check_error(DesktopUpdateCheckFailure::Network),
            AppError::new("desktopUpdateCheckNetworkFailed")
        );
        assert_eq!(
            desktop_update_check_error(DesktopUpdateCheckFailure::Other),
            AppError::new("desktopUpdateCheckFailed")
        );
    }

    #[test]
    fn desktop_update_download_retries_only_transient_failures() {
        for status in [408, 429, 500, 503, 599] {
            let network = tauri_plugin_updater::Error::Network(format!(
                "Download request failed with status: {status} response"
            ));
            assert_eq!(
                classify_desktop_update_download_error(&network),
                DesktopUpdateDownloadFailure::RetryableNetwork
            );
        }
        for status in [400, 401, 403, 404, 600] {
            let deterministic = tauri_plugin_updater::Error::Network(format!(
                "Download request failed with status: {status} response"
            ));
            assert_eq!(
                classify_desktop_update_download_error(&deterministic),
                DesktopUpdateDownloadFailure::Other
            );
        }
        assert!(!retryable_download_http_status("temporary failure"));
        assert!(!retryable_download_http_status(
            "Download request failed with status: unknown"
        ));
        assert!(should_retry_desktop_update_download(
            1,
            DesktopUpdateDownloadFailure::RetryableNetwork
        ));
        assert!(should_retry_desktop_update_download(
            DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS - 1,
            DesktopUpdateDownloadFailure::RetryableNetwork
        ));
        assert!(!should_retry_desktop_update_download(
            DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS,
            DesktopUpdateDownloadFailure::RetryableNetwork
        ));
        assert!(!should_retry_desktop_update_download(
            1,
            DesktopUpdateDownloadFailure::Trust
        ));

        let deterministic = tauri_plugin_updater::Error::EmptyEndpoints;
        assert_eq!(
            classify_desktop_update_download_error(&deterministic),
            DesktopUpdateDownloadFailure::Other
        );
    }

    #[test]
    fn desktop_update_download_errors_are_stable_and_hide_technical_details() {
        assert_eq!(
            desktop_update_download_error(DesktopUpdateDownloadFailure::RetryableNetwork),
            AppError::new("desktopUpdateNetworkFailed")
        );
        assert_eq!(
            desktop_update_download_error(DesktopUpdateDownloadFailure::Trust),
            AppError::new("desktopUpdateTrustInvalid")
        );
        assert_eq!(
            desktop_update_download_error(DesktopUpdateDownloadFailure::Other),
            AppError::new("desktopUpdateDownloadFailed")
        );
    }

    #[test]
    fn a_stale_harness_check_cannot_replace_an_installing_state() {
        let mut snapshot = LauncherSnapshot::initial("0.2.2");
        snapshot.harness_update = HarnessUpdateState::Installing {
            version: "0.1.0-rc.8".into(),
        };

        assert!(!replace_harness_update_if_checking(
            &mut snapshot,
            HarnessUpdateState::None
        ));

        assert_eq!(
            snapshot.harness_update,
            HarnessUpdateState::Installing {
                version: "0.1.0-rc.8".into()
            }
        );
    }

    #[test]
    fn a_current_harness_check_can_publish_its_result() {
        let mut snapshot = LauncherSnapshot::initial("0.2.2");
        snapshot.harness_update = HarnessUpdateState::Checking;

        assert!(replace_harness_update_if_checking(
            &mut snapshot,
            HarnessUpdateState::Available {
                version: "0.1.0-rc.8".into(),
            },
        ));

        assert_eq!(
            snapshot.harness_update,
            HarnessUpdateState::Available {
                version: "0.1.0-rc.8".into()
            }
        );
    }

    #[test]
    fn a_stale_harness_check_cannot_enter_checking() {
        let mut snapshot = LauncherSnapshot::initial("0.2.2");
        snapshot.phase = dsh_core::LauncherPhase::Ready;
        snapshot.harness_update = HarnessUpdateState::Installing {
            version: "0.1.0-rc.8".into(),
        };

        assert!(!mark_harness_update_checking(
            &mut snapshot,
            &HarnessUpdateState::Available {
                version: "0.1.0-rc.8".into(),
            }
        ));
        assert!(matches!(
            snapshot.harness_update,
            HarnessUpdateState::Installing { .. }
        ));
    }

    #[test]
    fn a_stopped_service_can_enter_harness_update_checking() {
        let mut snapshot = LauncherSnapshot::initial("0.2.2");
        snapshot.phase = dsh_core::LauncherPhase::Stopped;

        assert!(mark_harness_update_checking(
            &mut snapshot,
            &HarnessUpdateState::None
        ));
        assert_eq!(snapshot.harness_update, HarnessUpdateState::Checking);
    }

    #[test]
    fn ordinary_service_start_preserves_a_background_update_state() {
        let mut snapshot = LauncherSnapshot::initial("0.3.3");
        snapshot.harness_update = HarnessUpdateState::Downloaded {
            version: "0.2.0".into(),
        };

        complete_harness_deployment(&mut snapshot, "0.1.0".into(), false);

        assert_eq!(snapshot.harness_version.as_deref(), Some("0.1.0"));
        assert_eq!(
            snapshot.harness_update,
            HarnessUpdateState::Downloaded {
                version: "0.2.0".into()
            }
        );
    }

    #[test]
    fn forced_update_clears_the_completed_update_state() {
        let mut snapshot = LauncherSnapshot::initial("0.3.3");
        snapshot.harness_update = HarnessUpdateState::Installing {
            version: "0.2.0".into(),
        };

        complete_harness_deployment(&mut snapshot, "0.2.0".into(), true);

        assert_eq!(snapshot.harness_version.as_deref(), Some("0.2.0"));
        assert_eq!(snapshot.harness_update, HarnessUpdateState::None);
    }

    #[test]
    fn update_check_preserves_a_downloaded_candidate_until_a_newer_release_exists() {
        let current = Version::parse("0.1.0").unwrap();
        let downloaded = HarnessUpdateState::Downloaded {
            version: "0.2.0".into(),
        };
        assert_eq!(
            harness_update_after_check(
                downloaded.clone(),
                &current,
                &Version::parse("0.2.0").unwrap()
            ),
            downloaded
        );
        assert_eq!(
            harness_update_after_check(
                HarnessUpdateState::Downloaded {
                    version: "0.2.0".into()
                },
                &current,
                &Version::parse("0.3.0").unwrap()
            ),
            HarnessUpdateState::Available {
                version: "0.3.0".into()
            }
        );
    }

    #[test]
    fn stale_registry_result_does_not_erase_a_known_update() {
        let known = HarnessUpdateState::Available {
            version: "0.3.0".into(),
        };
        assert_eq!(
            harness_update_after_check(
                known.clone(),
                &Version::parse("0.1.0").unwrap(),
                &Version::parse("0.2.0").unwrap()
            ),
            known
        );
    }

    #[test]
    fn instance_lock_allows_only_one_launcher_per_desktop_home() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let first = acquire_instance_lock(&paths).unwrap();
        let error = acquire_instance_lock_with_timeout(&paths, Duration::ZERO).unwrap_err();
        assert_eq!(error.code, "launcherAlreadyRunning");
        drop(first);
        acquire_instance_lock(&paths).unwrap();
    }

    #[test]
    fn instance_lock_waits_for_a_restarting_launcher_to_exit() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let first = acquire_instance_lock(&paths).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(first);
        });

        acquire_instance_lock_with_timeout(&paths, Duration::from_secs(1)).unwrap();
        release.join().unwrap();
    }

    #[test]
    fn closing_or_requesting_exit_keeps_the_tray_process_running() {
        assert_eq!(
            lifecycle_decision(false, true),
            LifecycleDecision::KeepRunning
        );
    }

    #[test]
    fn closing_without_a_tray_quits_after_cleanup() {
        assert_eq!(
            lifecycle_decision(false, false),
            LifecycleDecision::QuitAfterCleanup
        );
    }

    #[test]
    fn cleanup_in_progress_does_not_allow_the_process_to_exit() {
        assert_eq!(
            lifecycle_decision(false, true),
            LifecycleDecision::KeepRunning
        );
        assert_eq!(
            lifecycle_decision(false, false),
            LifecycleDecision::QuitAfterCleanup
        );
    }

    #[test]
    fn completed_cleanup_allows_the_process_to_exit() {
        assert_eq!(lifecycle_decision(true, true), LifecycleDecision::AllowExit);
        assert_eq!(
            lifecycle_decision(true, false),
            LifecycleDecision::AllowExit
        );
    }
}
