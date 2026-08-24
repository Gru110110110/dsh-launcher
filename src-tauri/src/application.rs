use std::{
    fs::{File, OpenOptions},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dsh_core::{
    ActivityCode, ActivityState, AppError, AppResult, ApplicationPaths, DesktopUpdateState,
    HarnessUpdateMode, HarnessUpdateState, Language, LauncherPhase, LauncherSnapshot, LauncherStep,
    MigrationState, ProgressState, ThemePreference,
    browser::BrowserCatalog,
    marketplace::Marketplace,
    migration::MigrationService,
    preferences::Preferences,
    runtime::{
        DeploymentController, DeploymentEvent, activate_prepared_harness_update, deploy_runtime,
        discard_prepared_harness_update, installed_version, latest_harness_version,
        prepare_harness_update, recover_prepared_harness_update,
    },
    service::ServerManager,
    terminal::ensure_terminal_command,
};
use fs2::{FileExt, lock_contended_error};
use semver::Version;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{TrayIcon, TrayIconBuilder, TrayIconEvent},
};
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

fn external_link_url(target: &str) -> Option<&'static str> {
    match target {
        "github" => Some(GITHUB_REPOSITORY),
        "harnessGithub" => Some(HARNESS_GITHUB_REPOSITORY),
        "deepseek" => Some(DEEPSEEK_PLATFORM),
        _ => None,
    }
}

pub(crate) struct AppState {
    app: AppHandle,
    paths: ApplicationPaths,
    _instance_lock: File,
    snapshot: Mutex<LauncherSnapshot>,
    preferences: Mutex<Preferences>,
    browsers: BrowserCatalog,
    server: Mutex<ServerManager>,
    deployment: Mutex<Option<DeploymentController>>,
    background_update: Mutex<Option<DeploymentController>>,
    migration: MigrationService,
    pub(crate) marketplace: Marketplace,
    desktop_update_busy: AtomicBool,
    harness_update_check_busy: AtomicBool,
    startup_thread: Mutex<Option<thread::JoinHandle<()>>>,
    background_update_thread: Mutex<Option<thread::JoinHandle<()>>>,
    quitting: AtomicBool,
    exit_ready: AtomicBool,
    tray: Mutex<Option<TrayIcon>>,
}

impl AppState {
    fn new(app: AppHandle, paths: ApplicationPaths) -> AppResult<Arc<Self>> {
        let instance_lock = acquire_instance_lock(&paths)?;
        let preferences = Preferences::load(&paths.preferences_file, &paths.language_file);
        let browsers = BrowserCatalog::discover();
        let mut snapshot = LauncherSnapshot::initial(env!("CARGO_PKG_VERSION"));
        snapshot.language = preferences.language;
        snapshot.theme = preferences.theme;
        snapshot.browsers = browsers.choices();
        snapshot.selected_browser_id = if browsers.contains(&preferences.browser_id) {
            preferences.browser_id.clone()
        } else {
            "system".into()
        };
        snapshot.harness_version = installed_version(&paths);
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
        Ok(Arc::new(Self {
            app,
            _instance_lock: instance_lock,
            server: Mutex::new(ServerManager::new(paths.clone())),
            paths,
            snapshot: Mutex::new(snapshot),
            preferences: Mutex::new(preferences),
            browsers,
            deployment: Mutex::new(None),
            background_update: Mutex::new(None),
            migration,
            marketplace,
            desktop_update_busy: AtomicBool::new(false),
            harness_update_check_busy: AtomicBool::new(false),
            startup_thread: Mutex::new(None),
            background_update_thread: Mutex::new(None),
            quitting: AtomicBool::new(false),
            exit_ready: AtomicBool::new(false),
            tray: Mutex::new(None),
        }))
    }

    pub(crate) fn snapshot(&self) -> LauncherSnapshot {
        self.snapshot.lock().expect("snapshot poisoned").clone()
    }

    fn mutate(&self, update: impl FnOnce(&mut LauncherSnapshot)) {
        let _ = self.mutate_if(|snapshot| {
            update(snapshot);
            true
        });
    }

    fn mutate_if(&self, update: impl FnOnce(&mut LauncherSnapshot) -> bool) -> bool {
        let value = {
            let mut snapshot = self.snapshot.lock().expect("snapshot poisoned");
            if !update(&mut snapshot) {
                return false;
            }
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.clone()
        };
        let _ = self.app.emit("launcher://state", value);
        true
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
        let had_pending_market_change = self.marketplace.has_pending_rollback();
        let restarted = {
            let mut server = self.server.lock().expect("server poisoned");
            if let Err(error) = server.stop() {
                self.fail(error.clone());
                return Err(error);
            }
            server.start()
        };
        match restarted {
            Ok(url) => {
                if let Err(error) = self.marketplace.clear_pending_verification_while_guarded() {
                    log::warn!("could not clear verified marketplace rollback state: {error}");
                }
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
                let plugins = self.marketplace.pending_change_summary();
                log::error!(
                    "Harness did not publish an address with an unverified marketplace batch; rolling back: {error}"
                );
                if let Err(rollback_error) = self.marketplace.rollback_pending_while_guarded() {
                    self.fail(rollback_error.clone());
                    return Err(rollback_error);
                }
                match self.server.lock().expect("server poisoned").start() {
                    Ok(url) => {
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
                    snapshot.harness_update = previous_update;
                    snapshot.error = Some(failure.clone());
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
        let notify = move |event| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            match event {
                DeploymentEvent::Activity { code, values } => state.mutate(|snapshot| {
                    snapshot.activity = Some(ActivityState {
                        code,
                        values,
                        started_at_ms: now_ms(),
                    });
                    snapshot.progress = ProgressState::Indeterminate;
                }),
                DeploymentEvent::Progress { done, total } => state.mutate(|snapshot| {
                    snapshot.progress = total
                        .filter(|total| *total > 0)
                        .map_or(ProgressState::Indeterminate, |total| {
                            ProgressState::Determinate { done, total }
                        })
                }),
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
            deploy_runtime(
                &self.paths,
                force,
                target_version.as_deref(),
                controller,
                notify,
            )
        };
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        match deployed {
            Ok(version) => {
                self.mutate(|snapshot| complete_harness_deployment(snapshot, version, force));
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
                if let Err(error) = self.marketplace.clear_pending_verification_while_guarded() {
                    log::warn!("could not clear verified marketplace rollback state: {error}");
                }
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
                if self.marketplace.has_pending_rollback()
                    && should_rollback_marketplace_after_start_failure(
                        &error,
                        force || activate_prepared,
                    ) =>
            {
                let plugins = self.marketplace.pending_change_summary();
                log::error!(
                    "Harness did not publish an address with an unverified marketplace batch; rolling back: {error}"
                );
                if let Err(rollback_error) = self.marketplace.rollback_pending_while_guarded() {
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
            Err(error) => self.fail_unless_quitting(error),
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
            Ok(url) => self.mutate(|snapshot| {
                snapshot.phase = LauncherPhase::Ready;
                snapshot.step = LauncherStep::Start;
                snapshot.activity = None;
                snapshot.error = Some(update_error);
                snapshot.web_url = Some(url);
                snapshot.service_started_at_ms = Some(now_ms());
                snapshot.harness_version = installed_version(&self.paths);
                snapshot.harness_update = HarnessUpdateState::Failed { version };
            }),
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

        let controller = DeploymentController::default();
        let query_controller = controller.clone();
        let query =
            tauri::async_runtime::spawn_blocking(move || latest_harness_version(&query_controller));
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
        self.app
            .updater_builder()
            .on_before_exit(move || {
                // Tauri's Windows updater starts the installer and then calls
                // process::exit(0). Cleanup after Update::install is therefore
                // unreachable on Windows and must live in this hook.
                if let Err(error) = state.prepare_restart() {
                    log::error!("service cleanup before updater exit failed: {error:?}");
                }
            })
            .build()
            .map_err(|error| {
                log::error!("desktop updater configuration is invalid: {error:?}");
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
                    log::warn!("desktop update check request failed: {error:?}");
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
                log::warn!("desktop update check before download failed: {error:?}");
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
            let downloaded = tokio::time::timeout(
                DESKTOP_UPDATE_DOWNLOAD_TIMEOUT,
                update.download(
                    move |chunk, total| {
                        if let Some(state) = weak.upgrade() {
                            state.mutate(|snapshot| {
                                let done = match &snapshot.desktop_update {
                                    DesktopUpdateState::Downloading { done, .. } => *done,
                                    _ => 0,
                                }
                                .saturating_add(chunk as u64);
                                snapshot.desktop_update = DesktopUpdateState::Downloading {
                                    version: progress_version.clone(),
                                    done,
                                    total,
                                };
                            });
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
                            "desktop update download attempt {attempt}/{DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS} failed; will retry: {error:?}"
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
                        "desktop update download attempt {attempt}/{DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS} failed; giving up: {error:?}"
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
            log::warn!("desktop update install failed: {error:?}");
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
        let menu = tray_menu(&self.app, language)?;
        if let Some(tray) = self.tray.lock().expect("tray poisoned").as_ref() {
            tray.set_menu(Some(menu))?;
        }
        Ok(())
    }
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

fn tray_menu(app: &AppHandle, language: Language) -> tauri::Result<Menu<tauri::Wry>> {
    let (show, open_web, quit) = match language {
        Language::Zh => ("打开启动主页面", "打开 DeepSeek Harness 工作台", "退出"),
        Language::En => ("Open launcher", "Open DeepSeek Harness Workspace", "Quit"),
    };
    let show = MenuItem::with_id(app, "open", show, true, None::<&str>)?;
    let open_web = MenuItem::with_id(app, "web", open_web, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show, &open_web, &quit])
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window.show() {
            log::warn!("main window could not be shown: {error}");
            return;
        }
        if let Err(error) = window.set_focus() {
            log::warn!("main window could not be focused: {error}");
        }
    }
}

fn install_tray(app: &tauri::App, state: &Arc<AppState>) -> tauri::Result<()> {
    let menu = tray_menu(app.handle(), state.snapshot().language)?;
    let weak_menu = Arc::downgrade(state);
    let tray = TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().expect("bundle icon").clone())
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
                    "quit" => state.quit(),
                    _ => {}
                }
            }
        })
        .build(app)?;
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

pub fn run() {
    if dsh_core::service::handle_service_guard_cli() || handle_cli_probe() {
        return;
    }
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let paths = ApplicationPaths::from_environment().map_err(|error| error.to_string())?;
            paths.ensure_dirs().map_err(|error| error.to_string())?;
            let state =
                AppState::new(app.handle().clone(), paths).map_err(|error| error.to_string())?;
            if let Err(error) = install_tray(app, &state) {
                log::warn!("system tray unavailable; closing the window will exit: {error}");
            }
            app.manage(Arc::clone(&state));
            state
                .start(false, None)
                .map_err(|error| error.to_string())?;
            tauri::async_runtime::spawn(check_desktop_update_after_startup(state));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && let Some(state) = window.app_handle().try_state::<Arc<AppState>>()
            {
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
            commands::application_check_update,
            commands::application_install_update,
            commands::market_get_catalog,
            commands::market_refresh_catalog,
            commands::market_refresh_if_stale,
            commands::market_query,
            commands::market_installed,
            commands::market_compatibility,
            commands::market_inspect,
            commands::market_install,
            commands::market_uninstall,
            commands::market_pending_verification,
            commands::market_operation_busy,
            commands::market_rollback_pending,
            commands::market_open_plugin_github,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DSH Launcher")
        .run(|app, event| match event {
            #[cfg(target_os = "macos")]
            RunEvent::Reopen {
                has_visible_windows,
                ..
            } => {
                if !has_visible_windows {
                    show_main_window(app);
                }
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::{
        DEEPSEEK_PLATFORM, DESKTOP_UPDATE_DOWNLOAD_ATTEMPTS, DesktopUpdateCheckFailure,
        DesktopUpdateDownloadFailure, GITHUB_REPOSITORY, HARNESS_GITHUB_REPOSITORY,
        LifecycleDecision, WEBSITE, acquire_instance_lock, acquire_instance_lock_with_timeout,
        classify_desktop_update_check_error, classify_desktop_update_download_error,
        complete_harness_deployment, desktop_update_check_error, desktop_update_download_error,
        desktop_update_start_state, external_link_url, harness_update_after_check,
        lifecycle_decision, mark_harness_update_checking, replace_harness_update_if_checking,
        retryable_download_http_status, should_retry_desktop_update_download,
        should_rollback_marketplace_after_start_failure,
    };
    use dsh_core::{
        AppError, ApplicationPaths, DesktopUpdateState, HarnessUpdateState, LauncherSnapshot,
    };
    use semver::Version;

    #[test]
    fn product_website_uses_the_public_homepage() {
        assert_eq!(WEBSITE, "https://dsdesktop.com/");
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
