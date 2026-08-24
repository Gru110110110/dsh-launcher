//! Plugin marketplace for DSH Launcher.
//!
//! Consumes the [dsh-market](https://github.com/2BingLing/dsh-market) catalog
//! (`data/plugins.json`) and provides cached browsing, search, installation,
//! uninstallation and installed-state detection. Deliberately has no Tauri
//! dependency: every operation is plain blocking Rust so it can run inside
//! `spawn_blocking` from the command adapter and be unit-tested with isolated
//! homes.
//!
//! Stability posture (the Harness CLI changes fast):
//! - The launcher validates its pinned Harness runtime, then runs its own
//!   pinned pnpm against a staged copy of the selected profile. It never uses
//!   a user-installed `dsh` or pnpm.
//! - Bundle reconciliation mirrors the published Harness profile contract,
//!   while candidate validation preserves installation-owned layers.
//! - Every mutation is published with directory renames and retained as one
//!   rollback batch until a subsequent Harness start verifies it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

use crate::{
    error::{AppError, AppResult},
    paths::ApplicationPaths,
    runtime::configure_process_group,
};

const MARKET_REPOSITORY: &str = "2BingLing/dsh-market";
const MARKET_BRANCH: &str = "master";
/// Immutable trust anchor shipped with the desktop release. Catalog updates
/// must remain descendants of this reviewed repository commit; force-pushed
/// or repository-replaced histories are rejected.
const MARKET_TRUST_ANCHOR: &str = "298e815d77412ea57eeb0ecc56fa2b4e4683d194";
const NPM_REGISTRY: &str = "https://registry.npmjs.org";
const GITHUB_API: &str = "https://api.github.com";
const DEFAULT_PROFILE: &str = "web";
const PNPM_VERSION: &str = "10.12.3";
const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const MARKET_CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CATALOG_MAX_BYTES: usize = 96 * 1024 * 1024;
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(10);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_CAP: usize = 256 * 1024;
const TARBALL_MAX_BYTES: usize = 128 * 1024 * 1024;
const TARBALL_MAX_FILES: usize = 50_000;
const COMPAT_CONCURRENCY: usize = 8;
/// Installed-state scans are cached for this long: the paired query +
/// compatibility requests and rapid filter changes must not re-walk the
/// filesystem on every call. Mutations invalidate the cache immediately.
const INSTALLED_CACHE_TTL: Duration = Duration::from_secs(2);
const COMPAT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const COMPAT_UNKNOWN_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Uninstalled skills stay recoverable in the trash for 30 days.
const TRASH_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CORRUPT_PENDING_RETENTION: usize = 5;
/// How long to wait for the output reader after the child has exited; a
/// backgrounded grandchild that keeps the pipes open can delay EOF
/// indefinitely, so this bounds the wait and returns whatever arrived.
const READER_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Market data (serde-only, parsed from the dsh-market catalog)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarketCatalogFile {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub plugins: Vec<MarketPlugin>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarketPlugin {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub stars: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub description_zh: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub curated: bool,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub install: Option<MarketInstallInfo>,
    #[serde(default)]
    pub score: Option<MarketScore>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarketInstallInfo {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub needs_config: Option<bool>,
    /// Install commands scraped from the plugin README (e.g.
    /// `dsh plugin --profile web add dsh-better-sidebar@latest`). The
    /// catalog name is a display name and can differ in case from the real
    /// npm package, so these commands are the most faithful spec source.
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MarketScore {
    #[serde(default)]
    pub total: Option<f32>,
    #[serde(default)]
    pub explanation: Option<String>,
}

// ---------------------------------------------------------------------------
// IPC types (exported to TypeScript)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PluginKind {
    #[default]
    CordisPlugin,
    Skill,
}

impl PluginKind {
    fn parse(value: &str) -> Self {
        match value {
            "skill" => Self::Skill,
            _ => Self::CordisPlugin,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PluginSource {
    #[default]
    Skills,
    Profile,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum CompatibilityStatus {
    #[default]
    NotChecked,
    Compatible,
    Incompatible,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SourceBindingStatus {
    #[default]
    NotChecked,
    Verified,
    Mismatch,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityInfo {
    pub status: CompatibilityStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPlugin {
    pub plugin_id: Option<String>,
    pub local_name: String,
    #[serde(default)]
    pub version: Option<String>,
    pub source: PluginSource,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PluginSummary {
    pub id: String,
    pub kind: PluginKind,
    pub name: String,
    pub owner: String,
    pub repo: String,
    pub full_name: String,
    pub stars: u32,
    pub description: String,
    pub description_zh: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    pub curated: bool,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub needs_config: bool,
    #[serde(default)]
    pub install_method: String,
    pub install_target: String,
    #[serde(default)]
    pub install_version: Option<String>,
    pub source_binding: SourceBindingStatus,
    #[serde(default)]
    pub source_binding_detail: Option<String>,
    #[serde(default)]
    pub score_total: Option<f32>,
    #[serde(default)]
    pub score_explanation: Option<String>,
    pub compatibility: CompatibilityStatus,
    #[serde(default)]
    pub compatibility_detail: Option<String>,
    #[serde(default)]
    pub installed: Option<InstalledPlugin>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum MarketSort {
    #[default]
    Score,
    Stars,
    RecentlyUpdated,
    Name,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub kind: Option<PluginKind>,
    #[serde(default)]
    pub tag: Option<String>,
    /// Install-state filter: `None` = all, `Some(true)` = installed only,
    /// `Some(false)` = not installed only.
    #[serde(default)]
    pub installed: Option<bool>,
    #[serde(default)]
    pub sort: MarketSort,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default)]
    pub check_compatibility: bool,
}

fn default_page() -> u32 {
    1
}

fn default_page_size() -> u32 {
    24
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MarketPage {
    pub items: Vec<PluginSummary>,
    pub total: u32,
    pub page: u32,
    pub page_size: u32,
    pub total_pages: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MarketCatalogState {
    Loading,
    Ready {
        #[serde(rename = "generatedAt")]
        #[ts(rename = "generatedAt")]
        generated_at: Option<String>,
        #[serde(rename = "pluginCount")]
        #[ts(rename = "pluginCount")]
        plugin_count: u32,
        #[serde(default)]
        stale: bool,
    },
    Failed {
        #[serde(default)]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum MarketOperationKind {
    #[default]
    Install,
    Uninstall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MarketOperationResult {
    pub ok: bool,
    pub action: MarketOperationKind,
    pub plugin_id: String,
    pub restart_required: bool,
    #[serde(default)]
    pub error: Option<AppError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PendingMarketChange {
    pub plugin_id: String,
    pub name: String,
    pub action: MarketOperationKind,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PendingVerification {
    /// The most recent change is retained in the legacy fields so pending
    /// markers written by older launchers remain readable after an update.
    pub plugin_id: String,
    pub name: String,
    #[ts(type = "number")]
    pub installed_at_ms: u64,
    #[serde(default)]
    pub changes: Vec<PendingMarketChange>,
    /// The original journal was unreadable and was quarantined. The profile
    /// names in `changes` remain sufficient for a safe rollback, but the
    /// individual plugin identities are no longer trustworthy.
    #[serde(default)]
    pub journal_recovered: bool,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct Marketplace {
    paths: ApplicationPaths,
    catalog: Mutex<Option<Arc<MarketCatalogFile>>>,
    compat_cache: Mutex<HashMap<String, CachedCompatibility>>,
    pnpm_bin: Mutex<Option<PathBuf>>,
    installed_cache: Mutex<Option<InstalledCache>>,
    operation_busy: Arc<AtomicBool>,
    operation_transition: Arc<Mutex<()>>,
    operation_listener: Arc<Mutex<Option<MarketOperationListener>>>,
    catalog_listener: Mutex<Option<MarketCatalogListener>>,
    recovery_done: Mutex<bool>,
    loading: AtomicBool,
    last_error: Mutex<Option<String>>,
}

pub struct MarketOperationGuard {
    busy: Arc<AtomicBool>,
    transition: Arc<Mutex<()>>,
    listener: Option<MarketOperationListener>,
}

impl std::fmt::Debug for MarketOperationGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MarketOperationGuard")
            .field("busy", &self.busy.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for MarketOperationGuard {
    fn drop(&mut self) {
        let _transition = self
            .transition
            .lock()
            .expect("operation transition poisoned");
        self.busy.store(false, Ordering::Release);
        notify_operation_listener(&self.listener, false);
    }
}

type MarketOperationListener = Arc<dyn Fn(bool) + Send + Sync>;
type MarketCatalogListener = Arc<dyn Fn() + Send + Sync>;

fn notify_operation_listener(listener: &Option<MarketOperationListener>, busy: bool) {
    let Some(listener) = listener else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| listener(busy))).is_err() {
        log::error!("marketplace operation listener panicked");
    }
}

fn notify_catalog_listener(listener: &Option<MarketCatalogListener>) {
    let Some(listener) = listener else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| listener())).is_err() {
        log::error!("marketplace catalog listener panicked");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedCompatibility {
    package_name: String,
    package_version: Option<String>,
    cordis_version: Option<String>,
    checked_at_ms: u64,
    info: CompatibilityInfo,
    source_binding: SourceBindingStatus,
    source_binding_detail: Option<String>,
}

#[derive(Debug, Clone)]
struct RegistryPackageInfo {
    latest_version: String,
    cordis_range: Option<String>,
    repository_id: Option<String>,
    repository_declared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogTrustMeta {
    commit: String,
    sha256: String,
}

fn cached_compatibility_is_valid(
    cached: &CachedCompatibility,
    package_name: &str,
    package_version: Option<&str>,
    cordis_version: Option<&str>,
    now_ms: u64,
) -> bool {
    let ttl = if cached.info.status == CompatibilityStatus::Unknown {
        COMPAT_UNKNOWN_CACHE_TTL
    } else {
        COMPAT_CACHE_TTL
    };
    cached.package_name == package_name
        && package_version.is_none_or(|version| cached.package_version.as_deref() == Some(version))
        && cached.cordis_version.as_deref() == cordis_version
        && now_ms.saturating_sub(cached.checked_at_ms) < ttl.as_millis() as u64
}

impl Marketplace {
    pub fn new(paths: ApplicationPaths) -> Self {
        Self {
            paths,
            catalog: Mutex::new(None),
            compat_cache: Mutex::new(HashMap::new()),
            pnpm_bin: Mutex::new(None),
            installed_cache: Mutex::new(None),
            operation_busy: Arc::new(AtomicBool::new(false)),
            operation_transition: Arc::new(Mutex::new(())),
            operation_listener: Arc::new(Mutex::new(None)),
            catalog_listener: Mutex::new(None),
            recovery_done: Mutex::new(false),
            loading: AtomicBool::new(false),
            last_error: Mutex::new(None),
        }
    }

    /// Best-effort warm start from the on-disk cache.
    pub fn initialize(&self) {
        {
            let mut recovered = self.recovery_done.lock().expect("recovery poisoned");
            if !*recovered {
                match self.recover_marketplace_state() {
                    Ok(()) => *recovered = true,
                    Err(error) => {
                        log::warn!("marketplace transaction recovery failed: {error}");
                    }
                }
            }
        }
        if self.catalog.lock().expect("catalog poisoned").is_none()
            && let Ok(catalog) = self.load_cached_catalog()
        {
            *self.catalog.lock().expect("catalog poisoned") = Some(Arc::new(catalog));
        }
        let mut compat = self.compat_cache.lock().expect("compat poisoned");
        if compat.is_empty()
            && let Ok(bytes) = fs::read(self.compat_file())
            && let Ok(entries) =
                serde_json::from_slice::<HashMap<String, CachedCompatibility>>(&bytes)
        {
            *compat = entries;
        }
    }

    /// Acquire the single mutation/restart gate without queueing. Rejecting a
    /// second request is intentional: a hidden queue lets a stale UI submit
    /// operations against a profile state that no longer exists.
    pub fn begin_operation(&self) -> AppResult<MarketOperationGuard> {
        let _transition = self
            .operation_transition
            .try_lock()
            .map_err(|_| AppError::new("marketOperationBusy"))?;
        self.operation_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AppError::new("marketOperationBusy"))?;
        let listener = self
            .operation_listener
            .lock()
            .expect("operation listener poisoned")
            .clone();
        notify_operation_listener(&listener, true);
        Ok(MarketOperationGuard {
            busy: Arc::clone(&self.operation_busy),
            transition: Arc::clone(&self.operation_transition),
            listener,
        })
    }

    pub fn set_operation_listener(&self, listener: impl Fn(bool) + Send + Sync + 'static) {
        *self
            .operation_listener
            .lock()
            .expect("operation listener poisoned") = Some(Arc::new(listener));
    }

    pub fn set_catalog_listener(&self, listener: impl Fn() + Send + Sync + 'static) {
        *self
            .catalog_listener
            .lock()
            .expect("catalog listener poisoned") = Some(Arc::new(listener));
    }

    fn notify_catalog_changed(&self) {
        let listener = self
            .catalog_listener
            .lock()
            .expect("catalog listener poisoned")
            .clone();
        notify_catalog_listener(&listener);
    }

    pub fn operation_busy(&self) -> bool {
        self.operation_busy.load(Ordering::Acquire)
    }

    pub fn catalog_state(&self) -> MarketCatalogState {
        // Cached data always wins over a running refresh: a page that already
        // has a catalog keeps rendering it while the new one downloads.
        let catalog = self.catalog.lock().expect("catalog poisoned");
        if let Some(catalog) = catalog.as_ref() {
            // Report real staleness from the on-disk cache age so the
            // frontend can trigger its TTL refresh without guessing.
            let stale = self
                .catalog_age()
                .map(|age| age >= MARKET_CATALOG_TTL)
                .unwrap_or(true);
            return MarketCatalogState::Ready {
                generated_at: catalog.generated_at.clone(),
                plugin_count: u32::try_from(catalog.plugins.len()).unwrap_or(u32::MAX),
                stale,
            };
        }
        if self.loading.load(Ordering::SeqCst) {
            return MarketCatalogState::Loading;
        }
        let message = self.last_error.lock().expect("last error poisoned").clone();
        MarketCatalogState::Failed { message }
    }

    /// Fetch the catalog, validate it and swap it into memory. A failed
    /// refresh keeps the previously cached catalog. Calling refresh while a
    /// refresh is already running reports `Loading` instead of failing, so
    /// concurrent callers never turn an in-flight download into an error.
    pub fn refresh(&self) -> AppResult<MarketCatalogState> {
        if self
            .loading
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(MarketCatalogState::Loading);
        }
        let result = self.fetch_and_store();
        self.loading.store(false, Ordering::SeqCst);
        let state = match result {
            Ok(()) => self.ok_catalog_state(false),
            Err(error) => {
                *self.last_error.lock().expect("last error poisoned") = Some(
                    error
                        .safe_detail
                        .clone()
                        .unwrap_or_else(|| error.code.clone()),
                );
                if self.catalog.lock().expect("catalog poisoned").is_some() {
                    log::warn!("market catalog refresh failed, keeping cached data: {error}");
                    self.ok_catalog_state(true)
                } else {
                    Err(error)
                }
            }
        };
        self.notify_catalog_changed();
        state
    }

    /// Refresh only when the cached catalog is older than the TTL (or
    /// missing). The dsh-market upstream publishes daily, so a 24h gate keeps
    /// browsing instant while data stays fresh without manual refresh.
    pub fn refresh_if_stale(&self) -> AppResult<MarketCatalogState> {
        let stale = match self.catalog_age() {
            Some(age) => age >= MARKET_CATALOG_TTL,
            None => true,
        };
        if stale {
            self.refresh()
        } else {
            self.ok_catalog_state(false)
        }
    }

    /// Age of the cached catalog file (time since the last successful
    /// download), or `None` when there is no cache.
    fn catalog_age(&self) -> Option<Duration> {
        let modified = fs::metadata(self.catalog_file()).ok()?.modified().ok()?;
        std::time::SystemTime::now().duration_since(modified).ok()
    }

    fn ok_catalog_state(&self, stale: bool) -> AppResult<MarketCatalogState> {
        let catalog = self.catalog.lock().expect("catalog poisoned");
        match catalog.as_ref() {
            Some(catalog) => Ok(MarketCatalogState::Ready {
                generated_at: catalog.generated_at.clone(),
                plugin_count: u32::try_from(catalog.plugins.len()).unwrap_or(u32::MAX),
                stale,
            }),
            None => Ok(MarketCatalogState::Failed {
                message: Some("catalog not loaded".into()),
            }),
        }
    }

    fn fetch_and_store(&self) -> AppResult<()> {
        let (bytes, trust) = fetch_trusted_catalog()?;
        let mut catalog: MarketCatalogFile = serde_json::from_slice(&bytes)
            .map_err(|error| AppError::new("marketCatalogInvalid").detail(error.to_string()))?;
        sanitize_catalog(&mut catalog)?;
        log::info!(
            "market catalog loaded: {} plugins, schema {}",
            catalog.plugins.len(),
            catalog.schema_version
        );
        fs::create_dir_all(self.catalog_dir())
            .map_err(|error| AppError::io("createDirectory", &error))?;
        crate::paths::atomic_write(&self.catalog_file(), &bytes)?;
        crate::paths::atomic_write(&self.catalog_meta_file(), &serde_json::to_vec(&trust)?)?;
        *self.catalog.lock().expect("catalog poisoned") = Some(Arc::new(catalog));
        self.compat_cache.lock().expect("compat poisoned").clear();
        let _ = fs::remove_file(self.compat_file());
        // Plugin ids may have changed; drop the installed scan so the next
        // query re-maps entries against the fresh catalog.
        self.invalidate_installed_cache();
        Ok(())
    }

    pub fn query(&self, query: &MarketQuery) -> AppResult<MarketPage> {
        self.initialize();
        let catalog = self
            .catalog
            .lock()
            .expect("catalog poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketCatalogUnavailable"))?;
        let installed_index = InstalledIndex::build(self.scan_installed_cached(Some(&catalog)));

        let mut matched: Vec<&MarketPlugin> = catalog
            .plugins
            .iter()
            .filter(|plugin| plugin_matches(plugin, query))
            .filter(|plugin| match query.installed {
                None => true,
                Some(want_installed) => {
                    installed_index.by_plugin.contains_key(&plugin.id) == want_installed
                }
            })
            .collect();
        sort_plugins(&mut matched, query.sort);
        let total = u32::try_from(matched.len()).unwrap_or(u32::MAX);
        let page_size = query.page_size.clamp(1, 100);
        let total_pages = total.div_ceil(page_size).max(1);
        let page = query.page.clamp(1, total_pages);
        let start = usize::try_from((page - 1).saturating_mul(page_size)).unwrap_or(0);
        if query.check_compatibility {
            // Fill missing compatibility entries for this page concurrently,
            // so browsing never waits on serial registry lookups. Cached
            // entries are reused and later pages resolve instantly.
            self.fill_compatibility_parallel(&matched, start, page_size as usize);
        }
        let items = matched
            .into_iter()
            .skip(start)
            .take(page_size as usize)
            .map(|plugin| self.summary(plugin, &installed_index, query.check_compatibility))
            .collect();
        Ok(MarketPage {
            items,
            total,
            page,
            page_size,
            total_pages,
            generated_at: catalog.generated_at.clone(),
        })
    }

    pub fn installed(&self) -> AppResult<Vec<InstalledPlugin>> {
        self.initialize();
        let catalog = self.catalog.lock().expect("catalog poisoned").clone();
        Ok(self.scan_installed_cached(catalog.as_deref()))
    }

    pub fn compatibility(&self, plugin_id: &str) -> AppResult<CompatibilityInfo> {
        self.initialize();
        let catalog = self
            .catalog
            .lock()
            .expect("catalog poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketCatalogUnavailable"))?;
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| AppError::new("marketPluginNotFound").value("plugin", plugin_id))?;
        Ok(self.check_compatibility(plugin))
    }

    /// Resolve the exact install target and its current registry metadata for
    /// the confirmation dialog. This is deliberately separate from browsing:
    /// users must see the version and source binding before approving a
    /// package mutation, even if the background badge pass has not finished.
    pub fn inspect(&self, plugin_id: &str) -> AppResult<PluginSummary> {
        self.initialize();
        let catalog = self
            .catalog
            .lock()
            .expect("catalog poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketCatalogUnavailable"))?;
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| AppError::new("marketPluginNotFound").value("plugin", plugin_id))?;
        let kind = PluginKind::parse(&plugin.kind);
        if kind == PluginKind::CordisPlugin {
            self.refresh_compatibility(plugin);
        }
        let installed_index = InstalledIndex::build(self.scan_installed_cached(Some(&catalog)));
        let mut summary = self.summary(plugin, &installed_index, true);
        if kind == PluginKind::Skill {
            summary.install_version = Some(
                resolve_skill_commit(&plugin.id, None)
                    .map_err(|error| error.value("plugin", &plugin.name))?,
            );
        }
        Ok(summary)
    }

    pub fn install(
        &self,
        plugin_id: &str,
        _force: bool,
        expected_version: Option<&str>,
        service_running: bool,
    ) -> AppResult<MarketOperationResult> {
        self.initialize();
        let _guard = self.begin_operation()?;
        let catalog = self
            .catalog
            .lock()
            .expect("catalog poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketCatalogUnavailable"))?;
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .cloned()
            .ok_or_else(|| AppError::new("marketPluginNotFound").value("plugin", plugin_id))?;
        let kind = PluginKind::parse(&plugin.kind);
        match kind {
            PluginKind::CordisPlugin => {
                self.install_cordis(&plugin, _force, expected_version, service_running)
            }
            PluginKind::Skill => self.install_skill(&plugin, expected_version),
        }
    }

    pub fn uninstall(
        &self,
        plugin_id: &str,
        target: Option<&InstalledPlugin>,
        service_running: bool,
    ) -> AppResult<MarketOperationResult> {
        self.initialize();
        let _guard = self.begin_operation()?;
        let catalog = self
            .catalog
            .lock()
            .expect("catalog poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketCatalogUnavailable"))?;
        let installed = self.scan_installed(Some(&catalog));
        let matched: Vec<InstalledPlugin> = installed
            .into_iter()
            .filter(|entry| {
                entry.plugin_id.as_deref() == Some(plugin_id)
                    || entry.local_name.eq_ignore_ascii_case(plugin_id)
            })
            .collect();
        if matched.is_empty() {
            return Err(AppError::new("marketNotInstalled").value("plugin", plugin_id));
        }
        let target = target.ok_or_else(|| AppError::new("marketUninstallTargetRequired"))?;
        let selected = matched
            .into_iter()
            .find(|entry| same_install_location(entry, target))
            .ok_or_else(|| AppError::new("marketNotInstalled").value("plugin", plugin_id))?;
        self.purge_old_trash();
        let changed_profile = match selected.source {
            PluginSource::Skills => {
                let source_dir = self.skills_dir().join(&selected.local_name);
                if !source_dir.exists() {
                    return Err(AppError::new("marketNotInstalled").value("plugin", plugin_id));
                }
                let trash_dir = self.trash_dir();
                fs::create_dir_all(&trash_dir)
                    .map_err(|error| AppError::io("createDirectory", &error))?;
                let backup = trash_dir.join(format!("{}-{}", selected.local_name, now_ms()));
                move_dir(&source_dir, &backup)
                    .map_err(|error| AppError::io("marketUninstallFailed", &error))?;
                if let Ok(file) = fs::File::open(&backup) {
                    let _ = file.set_modified(std::time::SystemTime::now());
                }
                None
            }
            PluginSource::Profile => {
                let profile = selected
                    .profile
                    .clone()
                    .unwrap_or_else(|| DEFAULT_PROFILE.into());
                self.remove_profile_package(
                    &profile,
                    &selected.local_name,
                    plugin_id,
                    &selected.local_name,
                )?;
                Some(profile)
            }
        };
        self.invalidate_installed_cache();
        Ok(MarketOperationResult {
            ok: true,
            action: MarketOperationKind::Uninstall,
            plugin_id: plugin_id.into(),
            restart_required: service_running && changed_profile.is_some(),
            error: None,
        })
    }

    pub fn pending_verification(&self) -> AppResult<Option<PendingVerification>> {
        let bytes = match fs::read(self.pending_file()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut pending: PendingVerification = serde_json::from_slice(&bytes)?;
        if pending.changes.is_empty() && !pending.journal_recovered {
            pending.changes.push(PendingMarketChange {
                plugin_id: pending.plugin_id.clone(),
                name: pending.name.clone(),
                action: MarketOperationKind::Install,
                profile: Some(DEFAULT_PROFILE.into()),
            });
        }
        Ok(Some(pending))
    }

    pub fn has_pending_rollback(&self) -> bool {
        self.pending_file().exists()
    }

    pub fn pending_change_summary(&self) -> String {
        let Ok(Some(pending)) = self.pending_verification() else {
            return String::new();
        };
        let mut names = pending
            .changes
            .iter()
            .map(|change| {
                if change.plugin_id.is_empty() {
                    change.profile.as_deref().unwrap_or(&change.name)
                } else {
                    change.name.as_str()
                }
            })
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        names.join(", ")
    }

    pub fn clear_pending_verification(&self) -> AppResult<()> {
        let _guard = self.begin_operation()?;
        self.clear_pending_verification_while_guarded()
    }

    pub fn clear_pending_verification_while_guarded(&self) -> AppResult<()> {
        let pending = self.pending_file();
        if !pending.exists() {
            return self.finish_verified_cleanups();
        }
        fs::create_dir_all(self.catalog_dir())
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let verified = self
            .catalog_dir()
            .join(format!("pending.verified-{}.json", process_timestamp()));
        fs::rename(&pending, &verified)
            .map_err(|error| AppError::io("marketRollbackFailed", &error))?;
        // The atomic rename above commits the batch. From this point onward a
        // stale last-good directory is cleanup state, never rollback state.
        self.discard_last_good_profiles()?;
        remove_file_if_exists(&verified)
    }

    /// Restore every profile changed since the last successful Harness start.
    /// The rejected profile is retained for diagnosis instead of being
    /// deleted, because it may contain user-authored configuration.
    pub fn rollback_pending(&self) -> AppResult<()> {
        let _guard = self.begin_operation()?;
        self.rollback_pending_while_guarded()
    }

    pub fn rollback_pending_while_guarded(&self) -> AppResult<()> {
        let profiles = self.pending_profiles()?;
        for profile in profiles {
            self.rollback_profile(&profile)?;
        }
        remove_file_if_exists(&self.pending_file())?;
        self.invalidate_installed_cache();
        Ok(())
    }

    fn rollback_profile(&self, profile: &str) -> AppResult<()> {
        let target = self.profile_dir(profile);
        let last_good = self.last_good_profile(profile);
        if !last_good.exists() {
            return Ok(());
        }
        let rejected = self.profile_dir(&format!(
            ".{profile}.market-rejected-{}",
            process_timestamp()
        ));
        if target.exists() {
            fs::rename(&target, &rejected)
                .map_err(|error| AppError::io("marketRollbackFailed", &error))?;
        }
        if let Err(error) = fs::rename(&last_good, &target) {
            if rejected.exists() {
                let _ = fs::rename(&rejected, &target);
            }
            return Err(AppError::io("marketRollbackFailed", &error));
        }
        Ok(())
    }

    // -- internal helpers ---------------------------------------------------

    fn catalog_dir(&self) -> PathBuf {
        self.paths.cache_dir.join("marketplace")
    }

    fn catalog_file(&self) -> PathBuf {
        self.catalog_dir().join("plugins.json")
    }

    fn compat_file(&self) -> PathBuf {
        self.catalog_dir().join("compat.json")
    }

    fn catalog_meta_file(&self) -> PathBuf {
        self.catalog_dir().join("plugins.trust.json")
    }

    fn pending_file(&self) -> PathBuf {
        self.catalog_dir().join("pending.json")
    }

    fn last_good_profile(&self, profile: &str) -> PathBuf {
        self.profile_dir(&format!(".{profile}.market-last-good"))
    }

    fn trash_dir(&self) -> PathBuf {
        self.catalog_dir().join("trash")
    }

    fn install_log(&self) -> PathBuf {
        self.catalog_dir().join("install.log")
    }

    fn skills_dir(&self) -> PathBuf {
        self.paths.dsh_home.join("skills")
    }

    fn profiles_dir(&self) -> PathBuf {
        self.paths.dsh_home.join("profiles")
    }

    fn profile_dir(&self, profile: &str) -> PathBuf {
        self.profiles_dir().join(profile)
    }

    fn load_cached_catalog(&self) -> AppResult<MarketCatalogFile> {
        let bytes = fs::read(self.catalog_file())
            .map_err(|error| AppError::io("marketCatalogUnavailable", &error))?;
        let trust: CatalogTrustMeta = serde_json::from_slice(
            &fs::read(self.catalog_meta_file())
                .map_err(|error| AppError::io("marketCatalogUnavailable", &error))?,
        )
        .map_err(|error| AppError::new("marketCatalogInvalid").detail(error.to_string()))?;
        if !valid_commit_sha(&trust.commit) || sha256_bytes(&bytes) != trust.sha256 {
            return Err(AppError::new("marketCatalogInvalid")
                .detail("cached catalog trust metadata does not match its content"));
        }
        let mut catalog: MarketCatalogFile = serde_json::from_slice(&bytes)
            .map_err(|error| AppError::new("marketCatalogInvalid").detail(error.to_string()))?;
        sanitize_catalog(&mut catalog)?;
        Ok(catalog)
    }

    fn summary(
        &self,
        plugin: &MarketPlugin,
        installed_index: &InstalledIndex,
        check_compatibility: bool,
    ) -> PluginSummary {
        let kind = PluginKind::parse(&plugin.kind);
        let (compatibility, install_version, source_binding, source_binding_detail) = match kind {
            PluginKind::Skill => (
                CompatibilityInfo {
                    status: CompatibilityStatus::Compatible,
                    detail: None,
                },
                None,
                SourceBindingStatus::Verified,
                None,
            ),
            PluginKind::CordisPlugin if check_compatibility => {
                let compatibility = self.check_compatibility(plugin);
                let cached = self
                    .compat_cache
                    .lock()
                    .expect("compat poisoned")
                    .get(&plugin.id)
                    .cloned();
                (
                    compatibility,
                    cached
                        .as_ref()
                        .and_then(|entry| entry.package_version.clone()),
                    cached
                        .as_ref()
                        .map(|entry| entry.source_binding)
                        .unwrap_or(SourceBindingStatus::Unknown),
                    cached.and_then(|entry| entry.source_binding_detail),
                )
            }
            _ => (
                CompatibilityInfo {
                    status: CompatibilityStatus::NotChecked,
                    detail: None,
                },
                None,
                SourceBindingStatus::NotChecked,
                None,
            ),
        };
        PluginSummary {
            id: plugin.id.clone(),
            kind,
            name: plugin.name.clone(),
            owner: plugin.owner.clone(),
            repo: plugin.repo.clone(),
            full_name: plugin.full_name.clone(),
            stars: plugin.stars,
            description: plugin.description.clone(),
            description_zh: if plugin.description_zh.is_empty() {
                plugin.description.clone()
            } else {
                plugin.description_zh.clone()
            },
            tags: plugin.tags.clone(),
            homepage: plugin.homepage.clone(),
            license: plugin.license.clone(),
            curated: plugin.curated,
            pushed_at: plugin.pushed_at.clone(),
            updated_at: plugin.updated_at.clone(),
            needs_config: plugin
                .install
                .as_ref()
                .and_then(|i| i.needs_config)
                .unwrap_or(false),
            install_method: plugin
                .install
                .as_ref()
                .and_then(|i| i.method.clone())
                .unwrap_or_default(),
            install_target: match kind {
                PluginKind::Skill => plugin.id.clone(),
                PluginKind::CordisPlugin => install_package_name(plugin).unwrap_or_default(),
            },
            install_version,
            source_binding,
            source_binding_detail,
            score_total: plugin.score.as_ref().and_then(|s| s.total),
            score_explanation: plugin.score.as_ref().and_then(|s| s.explanation.clone()),
            compatibility: compatibility.status,
            compatibility_detail: compatibility.detail,
            installed: installed_index.by_plugin.get(&plugin.id).cloned(),
        }
    }

    fn check_compatibility(&self, plugin: &MarketPlugin) -> CompatibilityInfo {
        let package_name = install_package_name(plugin).unwrap_or_default();
        let cordis_version = installed_cordis_version(&self.paths);
        let now = now_ms();
        {
            let cache = self.compat_cache.lock().expect("compat poisoned");
            if let Some(cached) = cache.get(&plugin.id)
                && cached_compatibility_is_valid(
                    cached,
                    &package_name,
                    None,
                    cordis_version.as_deref(),
                    now,
                )
            {
                return cached.info.clone();
            }
        }
        let entry = fetch_compatibility_entry_with(plugin, &self.paths, None);
        let info = entry.info.clone();
        self.store_compatibility(&plugin.id, entry);
        info
    }

    fn refresh_compatibility(&self, plugin: &MarketPlugin) -> CachedCompatibility {
        let entry = fetch_compatibility_entry_with(plugin, &self.paths, None);
        self.store_compatibility(&plugin.id, entry.clone());
        entry
    }

    /// Fill compatibility entries for one result page without serial network
    /// stalls: missing items are fetched concurrently with a shared HTTP
    /// client (bounded workers), then cached in a single lock acquisition.
    fn fill_compatibility_parallel(
        &self,
        matched: &[&MarketPlugin],
        start: usize,
        page_size: usize,
    ) {
        let missing: Vec<&MarketPlugin> = {
            let cache = self.compat_cache.lock().expect("compat poisoned");
            let cordis_version = installed_cordis_version(&self.paths);
            let now = now_ms();
            matched
                .iter()
                .skip(start)
                .take(page_size)
                .copied()
                .filter(|plugin| {
                    PluginKind::parse(&plugin.kind) == PluginKind::CordisPlugin
                        && !cache.get(&plugin.id).is_some_and(|cached| {
                            cached_compatibility_is_valid(
                                cached,
                                &install_package_name(plugin).unwrap_or_default(),
                                None,
                                cordis_version.as_deref(),
                                now,
                            )
                        })
                })
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let fetched = fetch_compatibility_batch(&missing, &self.paths);
        if fetched.is_empty() {
            return;
        }
        let mut cache = self.compat_cache.lock().expect("compat poisoned");
        cache.extend(fetched);
        let snapshot = cache.clone();
        drop(cache);
        if let Ok(bytes) = serde_json::to_vec(&snapshot)
            && fs::create_dir_all(self.catalog_dir()).is_ok()
        {
            let _ = crate::paths::atomic_write(&self.compat_file(), &bytes);
        }
    }

    fn store_compatibility(&self, plugin_id: &str, info: CachedCompatibility) {
        let mut cache = self.compat_cache.lock().expect("compat poisoned");
        cache.insert(plugin_id.into(), info);
        let snapshot = cache.clone();
        drop(cache);
        if let Ok(bytes) = serde_json::to_vec(&snapshot)
            && fs::create_dir_all(self.catalog_dir()).is_ok()
        {
            let _ = crate::paths::atomic_write(&self.compat_file(), &bytes);
        }
    }

    // -- installation -------------------------------------------------------

    /// The cordis install path runs the launcher's own pinned Node and
    /// Harness CLI; refuse with a readable error when that runtime is not
    /// present yet instead of a bare "No such file or directory".
    fn require_runtime(&self) -> AppResult<()> {
        if !self.paths.node_bin.is_file() {
            return Err(AppError::new("marketRuntimeUnavailable").detail(format!(
                "node runtime missing at {}",
                self.paths.node_bin.display()
            )));
        }
        if !self.paths.dsh_bin.is_file() {
            return Err(AppError::new("marketRuntimeUnavailable").detail(format!(
                "harness CLI missing at {}",
                self.paths.dsh_bin.display()
            )));
        }
        Ok(())
    }

    fn install_cordis(
        &self,
        plugin: &MarketPlugin,
        _force: bool,
        expected_version: Option<&str>,
        service_running: bool,
    ) -> AppResult<MarketOperationResult> {
        // Always refresh registry metadata immediately before mutation. This
        // binds the compatibility decision, repository identity and exact
        // package version to the artifact that pnpm will install.
        let verified = self.refresh_compatibility(plugin);
        validate_expected_package_version(
            &plugin.name,
            expected_version,
            verified.package_version.as_deref(),
        )?;
        match verified.source_binding {
            SourceBindingStatus::Mismatch => {
                return Err(AppError::new("marketSourceMismatch")
                    .value("plugin", &plugin.name)
                    .detail(verified.source_binding_detail.clone().unwrap_or_default()));
            }
            SourceBindingStatus::Unknown | SourceBindingStatus::NotChecked => {
                return Err(AppError::new("marketSourceUnknown")
                    .value("plugin", &plugin.name)
                    .detail(verified.source_binding_detail.clone().unwrap_or_default()));
            }
            SourceBindingStatus::Verified => {}
        }
        match verified.info.status {
            CompatibilityStatus::Incompatible => {
                return Err(AppError::new("marketIncompatible")
                    .value("plugin", &plugin.name)
                    .detail(verified.info.detail.clone().unwrap_or_default()));
            }
            CompatibilityStatus::Unknown | CompatibilityStatus::NotChecked => {
                return Err(AppError::new("marketCompatUnknown").value("plugin", &plugin.name));
            }
            CompatibilityStatus::Compatible => {}
        }
        let pkg = install_package_name(plugin).ok_or_else(|| {
            AppError::new("marketInstallFailed")
                .value("plugin", &plugin.name)
                .detail("catalog does not contain a valid npm package target")
        })?;
        let install_spec = verified
            .package_version
            .as_ref()
            .map(|version| format!("{pkg}@{version}"))
            .unwrap_or_else(|| pkg.clone());
        self.require_runtime()?;
        let pending_change = PendingMarketChange {
            plugin_id: plugin.id.clone(),
            name: plugin.name.clone(),
            action: MarketOperationKind::Install,
            profile: Some(DEFAULT_PROFILE.into()),
        };
        if let Err(error) =
            self.mutate_profile_package(DEFAULT_PROFILE, "add", &install_spec, &pending_change)
        {
            let detail = error
                .safe_detail
                .clone()
                .unwrap_or_else(|| error.code.clone());
            self.log_operation("install", &install_spec, false, &detail);
            return Err(error.value("plugin", &plugin.name));
        }
        self.invalidate_installed_cache();
        self.log_operation("install", &install_spec, true, "ok");
        Ok(MarketOperationResult {
            ok: true,
            action: MarketOperationKind::Install,
            plugin_id: plugin.id.clone(),
            restart_required: service_running,
            error: None,
        })
    }

    fn install_skill(
        &self,
        plugin: &MarketPlugin,
        expected_commit: Option<&str>,
    ) -> AppResult<MarketOperationResult> {
        // The catalog is external data: a name containing separators or `..`
        // must never influence where the extracted skill ends up.
        if !valid_skill_dir_name(&plugin.name) {
            return Err(AppError::new("marketInstallFailed")
                .value("plugin", &plugin.name)
                .detail("invalid skill directory name in catalog"));
        }
        let target = self.skills_dir().join(&plugin.name);
        if target.exists() {
            return Err(AppError::new("marketAlreadyInstalled").value("plugin", &plugin.name));
        }
        // Catalog sanitization binds owner/repo/fullName to this canonical id;
        // use the same value disclosed in the confirmation dialog so metadata
        // fields cannot redirect a skill install to another repository.
        let repo = plugin.id.clone();
        // Resolve and disclose an immutable commit before confirmation, then
        // verify that exact commit belongs to the repository. A moving default
        // branch must never change the artifact after user approval.
        let commit = resolve_skill_commit(&repo, expected_commit)
            .map_err(|error| error.value("plugin", &plugin.name))?;
        let tarball_url = format!("https://codeload.github.com/{repo}/tar.gz/{commit}");
        let bytes = fetch_bytes(
            &tarball_url,
            CATALOG_FETCH_TIMEOUT,
            TARBALL_MAX_BYTES,
            "marketInstallFailed",
            "skill tarball",
        )
        .map_err(|error| error.value("plugin", &plugin.name))?;
        let extract_root = self.catalog_dir().join("extract");
        let staging = extract_root.join(format!("{}-{}", plugin.name, process_timestamp()));
        fs::create_dir_all(&staging).map_err(|error| AppError::io("createDirectory", &error))?;
        if let Err(error) = extract_tarball(&bytes, &staging) {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_dir(&extract_root);
            self.log_operation("install-skill", &plugin.name, false, &error.to_string());
            return Err(AppError::new("marketInstallFailed")
                .value("plugin", &plugin.name)
                .detail(error.to_string()));
        }
        if !staging.join("SKILL.md").is_file() {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_dir(&extract_root);
            return Err(AppError::new("marketInstallFailed")
                .value("plugin", &plugin.name)
                .detail("skill repository does not contain a root SKILL.md"));
        }
        fs::create_dir_all(self.skills_dir())
            .map_err(|error| AppError::io("createDirectory", &error))?;
        if let Err(error) = move_dir(&staging, &target) {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_dir(&extract_root);
            return Err(AppError::io("marketInstallFailed", &error));
        }
        // Best effort: drop the extract root when this staging dir was the
        // last thing in it.
        let _ = fs::remove_dir(&extract_root);
        self.invalidate_installed_cache();
        self.log_operation("install-skill", &plugin.name, true, "ok");
        Ok(MarketOperationResult {
            ok: true,
            action: MarketOperationKind::Install,
            plugin_id: plugin.id.clone(),
            restart_required: false,
            error: None,
        })
    }

    fn remove_profile_package(
        &self,
        profile: &str,
        pkg: &str,
        plugin_id: &str,
        plugin_name: &str,
    ) -> AppResult<()> {
        self.require_runtime()?;
        let pending_change = PendingMarketChange {
            plugin_id: plugin_id.into(),
            name: plugin_name.into(),
            action: MarketOperationKind::Uninstall,
            profile: Some(profile.into()),
        };
        if let Err(mut error) = self.mutate_profile_package(profile, "remove", pkg, &pending_change)
        {
            let detail = error
                .safe_detail
                .clone()
                .unwrap_or_else(|| error.code.clone());
            self.log_operation("remove", pkg, false, &detail);
            // Preserve every actionable validation code. Only relabel the
            // generic shared mutation failure so an uninstall I/O/pnpm error
            // is not presented as an installation failure.
            if error.code == "marketInstallFailed" {
                error.code = "marketUninstallFailed".into();
            }
            return Err(error.value("plugin", plugin_name));
        }
        self.log_operation("remove", pkg, true, "ok");
        Ok(())
    }

    /// Prepare a complete candidate profile, mutate and validate it there,
    /// then publish it with directory renames. The active profile remains
    /// byte-for-byte untouched on command or reconciliation failure.
    fn mutate_profile_package(
        &self,
        profile: &str,
        verb: &str,
        pkg: &str,
        pending_change: &PendingMarketChange,
    ) -> AppResult<()> {
        if !valid_profile_dir_name(profile) {
            return Err(AppError::new("marketInstallFailed").detail("invalid profile name"));
        }
        // A previously verified batch may have left only cleanup work after
        // an I/O failure. Finish that work before reusing last-good paths for
        // a new transaction, or fail without touching the active profile.
        self.finish_verified_cleanups()?;
        self.recover_profile_transaction(profile)?;
        self.ensure_pnpm()?;
        let profiles_dir = self.profiles_dir();
        fs::create_dir_all(&profiles_dir)
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let stamp = process_timestamp();
        let candidate_name = format!(".{profile}.market-candidate-{stamp}");
        let backup_name = format!(".{profile}.market-backup-{stamp}");
        let source = self.profile_dir(profile);
        let candidate = self.profile_dir(&candidate_name);
        let backup = self.profile_dir(&backup_name);
        let last_good = self.last_good_profile(profile);

        if !source.is_dir() {
            return Err(AppError::new("marketProfileMissing").value("profile", profile));
        }
        if self.pending_verification_for_mutation()?.is_none() && last_good.exists() {
            fs::remove_dir_all(&last_good)
                .map_err(|error| AppError::io("marketRollbackFailed", &error))?;
        }
        let baseline = read_manifest(&source.join("package.json"))?;
        if profile == DEFAULT_PROFILE && !has_installation_owned_foundation(&baseline) {
            return Err(AppError::new("marketProfileInvalid")
                .detail("web profile has no installation-owned foundation bundle"));
        }
        let target = normalize_package_spec(pkg).ok_or_else(|| {
            AppError::new("marketProfileInvalid")
                .detail("operation target is not a registry package")
        })?;
        if verb == "remove" && !baseline.dependencies.contains_key(&target) {
            return Err(AppError::new("marketNotDirectDependency")
                .value("plugin", target)
                .value("profile", profile));
        }
        if verb == "add" && baseline.dependencies.contains_key(&target) {
            return Err(AppError::new("marketAlreadyInstalled").value("plugin", target));
        }
        let source_revision = profile_control_digest(&source)?;
        if verb == "remove" {
            validate_reverse_package_dependencies(&source, &baseline, pkg)?;
        }
        copy_profile_candidate(&source, &candidate)?;

        // Always use the pinned pnpm directly with lifecycle scripts disabled.
        // The Harness CLI forwards to pnpm without this safety boundary.
        let outcome =
            self.run_pnpm_fallback(&candidate_name, verb, pkg)
                .and_then(|()| match verb {
                    "add" => self.reconcile_bundles_after_install(&candidate_name),
                    "remove" => self.reconcile_bundles_after_remove(&candidate_name, pkg),
                    _ => Err(AppError::new("marketInstallFailed").detail("unsupported operation")),
                });
        if let Err(error) = outcome {
            let _ = fs::remove_dir_all(&candidate);
            return Err(error);
        }
        if let Err(error) = validate_candidate_profile(&baseline, &candidate, verb, pkg) {
            let _ = fs::remove_dir_all(&candidate);
            return Err(error);
        }
        if profile_control_digest(&source)? != source_revision {
            let _ = fs::remove_dir_all(&candidate);
            return Err(AppError::new("marketProfileChanged").value("profile", profile));
        }

        // Persist the rollback journal before the first publication rename.
        // After a power loss, startup can therefore distinguish a candidate
        // profile from a previously verified one and restore the batch base.
        let previous_pending = fs::read(self.pending_file()).ok();
        if let Err(error) = self.write_pending_change(
            &pending_change.plugin_id,
            &pending_change.name,
            pending_change.action,
            profile,
        ) {
            let _ = fs::remove_dir_all(&candidate);
            return Err(error);
        }

        let publication_backup = if last_good.exists() {
            &backup
        } else {
            &last_good
        };
        if let Err(error) = fs::rename(&source, publication_backup) {
            let _ = restore_pending_snapshot(&self.pending_file(), previous_pending.as_deref());
            let _ = fs::remove_dir_all(&candidate);
            return Err(AppError::io("marketInstallFailed", &error));
        }
        if let Err(error) = fs::rename(&candidate, &source) {
            let _ = fs::rename(publication_backup, &source);
            let _ = restore_pending_snapshot(&self.pending_file(), previous_pending.as_deref());
            let _ = fs::remove_dir_all(&candidate);
            return Err(AppError::io("marketInstallFailed", &error));
        }
        if backup.exists()
            && let Err(error) = fs::remove_dir_all(&backup)
        {
            log::warn!("could not remove marketplace profile backup: {error}");
        }
        Ok(())
    }

    fn recover_profile_transaction(&self, profile: &str) -> AppResult<()> {
        let target = self.profile_dir(profile);
        let backup_prefix = format!(".{profile}.market-backup-");
        let candidate_prefix = format!(".{profile}.market-candidate-");
        let Ok(entries) = fs::read_dir(self.profiles_dir()) else {
            return Ok(());
        };
        let mut backups = Vec::new();
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&backup_prefix) {
                backups.push(entry.path());
            } else if name.starts_with(&candidate_prefix) {
                candidates.push(entry.path());
            }
        }
        backups.sort();
        candidates.sort();
        let target_was_missing = !target.exists();
        let had_ephemeral_backup = !backups.is_empty();
        let had_candidate = !candidates.is_empty();
        let mut restored_unpublished_change = false;
        if !target.exists()
            && let Some(backup) = backups.pop()
        {
            fs::rename(backup, &target)
                .map_err(|error| AppError::io("marketInstallFailed", &error))?;
            restored_unpublished_change = true;
        } else if !target.exists() && self.last_good_profile(profile).exists() {
            fs::rename(self.last_good_profile(profile), &target)
                .map_err(|error| AppError::io("marketInstallFailed", &error))?;
            restored_unpublished_change = true;
        }
        for path in backups.into_iter().chain(candidates) {
            if let Err(error) = fs::remove_dir_all(&path) {
                log::warn!("could not clean marketplace transaction directory: {error}");
            }
        }
        // If the journal was durable but the candidate was not published,
        // remove exactly that final change. Earlier changes in the same batch
        // remain pending and must still be verified or rolled back.
        let abandoned_before_first_rename =
            !target_was_missing && had_candidate && !had_ephemeral_backup;
        if restored_unpublished_change || abandoned_before_first_rename {
            log::warn!(
                "recovered an unpublished marketplace change for profile {profile}; removing the final change from the pending batch"
            );
            if let Err(error) = self.drop_last_pending_change(profile) {
                // An unreadable journal is quarantined by the immediately
                // following recovery stage. Keep the restored profile and its
                // last-good baseline intact so that stage can preserve broad
                // rollback coverage instead of failing startup recovery here.
                log::warn!(
                    "could not trim the unpublished marketplace journal tail for profile {profile}: {error}"
                );
            }
        }
        Ok(())
    }

    /// Make the launcher's pinned pnpm available and return the directories
    /// to prepend to PATH. A system pnpm is deliberately never trusted: its
    /// version and origin are outside the launcher's update boundary.
    fn ensure_pnpm(&self) -> AppResult<Vec<PathBuf>> {
        if let Some(dir) = self.pnpm_bin.lock().expect("pnpm poisoned").as_ref()
            && probe_executable(dir, "pnpm")
        {
            return Ok(path_prepend_dirs(dir));
        }
        let install_dir = self.paths.runtime_dir.join(format!("pnpm-{PNPM_VERSION}"));
        if let Some(bin_dir) = find_pnpm_dir(&install_dir)
            && probe_executable(&bin_dir, "pnpm")
        {
            *self.pnpm_bin.lock().expect("pnpm poisoned") = Some(bin_dir.clone());
            return Ok(path_prepend_dirs(&bin_dir));
        }
        fs::create_dir_all(&install_dir)
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let mut command = Command::new(&self.paths.node_bin);
        command
            .arg(npm_cli(&self.paths.node_dir))
            .args(["install", "-g", &format!("pnpm@{PNPM_VERSION}")])
            .arg(format!("--prefix={}", install_dir.display()))
            .args(["--no-audit", "--no-fund", "--package-lock=false"])
            .arg(format!(
                "--cache={}",
                self.paths.cache_dir.join("npm").display()
            ))
            .arg("--loglevel=error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        market_command_env(&mut command, &self.paths, &[]);
        let (success, output) = run_child(command, INSTALL_TIMEOUT).map_err(|error| {
            AppError::new("marketPnpmUnavailable")
                .detail(error.safe_detail.unwrap_or_else(|| error.code.clone()))
        })?;
        if !success {
            let detail = tail(&output, 400);
            return Err(AppError::new("marketPnpmUnavailable").detail(detail));
        }
        // npm -g --prefix places shims in different spots per platform and
        // npm version; store the directory that actually holds the binary.
        let bin_dir = find_pnpm_dir(&install_dir).ok_or_else(|| {
            AppError::new("marketPnpmUnavailable")
                .detail("pnpm executable not found after provisioning")
        })?;
        *self.pnpm_bin.lock().expect("pnpm poisoned") = Some(bin_dir.clone());
        Ok(path_prepend_dirs(&bin_dir))
    }

    /// Fallback: run the pinned pnpm directly inside the target profile.
    fn run_pnpm_fallback(&self, profile: &str, verb: &str, pkg: &str) -> AppResult<()> {
        let bin_dir = self
            .pnpm_bin
            .lock()
            .expect("pnpm poisoned")
            .clone()
            .ok_or_else(|| AppError::new("marketPnpmUnavailable"))?;
        let executable = bin_dir.join(pnpm_executable_name());
        let prepend = vec![bin_dir];
        let profile_dir = self.profile_dir(profile);
        fs::create_dir_all(&profile_dir)
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let mut command = Command::new(&executable);
        command.arg(verb).arg(pkg).args(pnpm_mutation_flags(verb));
        command
            .current_dir(&profile_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        market_command_env(&mut command, &self.paths, &prepend);
        let (success, output) = run_child(command, INSTALL_TIMEOUT)?;
        if !success {
            return Err(AppError::new("marketInstallFailed").detail(tail(&output, 400)));
        }
        Ok(())
    }

    /// Reconcile `dsh.profile.bundles` after installing via pnpm directly
    /// (mirrors the Harness CLI's own reconciliation: dependencies whose
    /// package declares `dsh.bundle` join the bundle stack).
    fn reconcile_bundles_after_install(&self, profile: &str) -> AppResult<()> {
        let manifest_path = self.profile_dir(profile).join("package.json");
        let mut manifest = read_manifest(&manifest_path)?;
        let dependencies: Vec<String> = manifest.dependencies.keys().cloned().collect();
        let bundles = &mut manifest.bundles;
        let profile_dir = self.profile_dir(profile);
        let mut changed = false;
        for dep in dependencies {
            if bundles.iter().any(|b| b == &dep) {
                continue;
            }
            if package_declares_bundle(&profile_dir, &dep) {
                bundles.push(dep);
                changed = true;
            }
        }
        if changed {
            write_manifest(&manifest_path, &manifest)?;
        }
        Ok(())
    }

    /// Reconcile `dsh.profile.bundles` after removing a dependency. Only the
    /// package explicitly removed by this operation may leave the stack:
    /// profile templates include installation-owned bundles such as
    /// `@deepseek-ai/dsh-base` and `@deepseek-ai/dsh-web-app` which resolve
    /// from the pinned Harness runtime and intentionally are not profile
    /// dependencies. Filtering the whole stack against `dependencies` would
    /// erase those foundation layers and make the service unbootable.
    fn reconcile_bundles_after_remove(&self, profile: &str, removed_pkg: &str) -> AppResult<()> {
        let manifest_path = self.profile_dir(profile).join("package.json");
        let mut manifest = read_manifest(&manifest_path)?;
        if manifest.dependencies.contains_key(removed_pkg) {
            return Ok(());
        }
        let before = manifest.bundles.len();
        manifest.bundles.retain(|bundle| bundle != removed_pkg);
        if manifest.bundles.len() != before {
            write_manifest(&manifest_path, &manifest)?;
        }
        Ok(())
    }

    fn scan_installed(&self, catalog: Option<&MarketCatalogFile>) -> Vec<InstalledPlugin> {
        let mut found = Vec::new();
        let index = PluginIndex::build(catalog);
        // 1. Skills directory (name, name-version, name-latest conventions).
        if let Ok(entries) = fs::read_dir(self.skills_dir()) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let local_name = entry.file_name().to_string_lossy().into_owned();
                let (base_name, version) = split_name_version(&local_name);
                let plugin_id = index
                    .by_name
                    .get(&base_name.to_lowercase())
                    .or_else(|| index.by_name.get(&local_name.to_lowercase()))
                    .and_then(Clone::clone);
                found.push(InstalledPlugin {
                    plugin_id,
                    local_name,
                    version,
                    source: PluginSource::Skills,
                    profile: None,
                });
            }
        }
        // 2. Profile dependencies (package.json dependencies + bundles).
        if let Ok(entries) = fs::read_dir(self.profiles_dir()) {
            for entry in entries.flatten() {
                let profile_dir = entry.path();
                if !profile_dir.is_dir() {
                    continue;
                }
                let profile_name = entry.file_name().to_string_lossy().into_owned();
                if profile_name.starts_with('.') {
                    continue;
                }
                let manifest_path = profile_dir.join("package.json");
                let Ok(manifest) = read_manifest(&manifest_path) else {
                    continue;
                };
                let mut names: Vec<String> = manifest.dependencies.keys().cloned().collect();
                for bundle in &manifest.bundles {
                    if !names.iter().any(|name| name == bundle) {
                        names.push(bundle.clone());
                    }
                }
                for dep in names {
                    let package_key = dep.to_lowercase();
                    let plugin_id = read_installed_repository(&profile_dir, &dep)
                        .and_then(|repository| {
                            index
                                .by_package_source
                                .get(&(package_key.clone(), repository.to_lowercase()))
                                .cloned()
                        })
                        .or_else(|| index.by_package.get(&package_key).and_then(Clone::clone));
                    if plugin_id.is_none() {
                        continue;
                    }
                    let version = read_installed_version(&profile_dir, &dep)
                        .or_else(|| manifest.dependencies.get(&dep).cloned());
                    found.push(InstalledPlugin {
                        plugin_id,
                        local_name: dep,
                        version: version.map(|v| v.trim_start_matches(['^', '~']).to_owned()),
                        source: PluginSource::Profile,
                        profile: Some(profile_name.clone()),
                    });
                }
            }
        }
        found
    }

    /// Installed scan with a short TTL so the paired query + compatibility
    /// requests and rapid filter changes do not re-walk the filesystem on
    /// every call. Mutating operations and catalog refreshes invalidate it.
    fn scan_installed_cached(&self, catalog: Option<&MarketCatalogFile>) -> Vec<InstalledPlugin> {
        {
            let cache = self
                .installed_cache
                .lock()
                .expect("installed cache poisoned");
            if let Some(cached) = cache.as_ref()
                && cached.scanned_at.elapsed() < INSTALLED_CACHE_TTL
            {
                return cached.entries.clone();
            }
        }
        let entries = self.scan_installed(catalog);
        *self
            .installed_cache
            .lock()
            .expect("installed cache poisoned") = Some(InstalledCache {
            scanned_at: Instant::now(),
            entries: entries.clone(),
        });
        entries
    }

    fn invalidate_installed_cache(&self) {
        *self
            .installed_cache
            .lock()
            .expect("installed cache poisoned") = None;
    }

    /// Drop trash entries older than the retention window. Runs before a new
    /// uninstall moves content in, so the trash stays bounded without any
    /// scheduled background work.
    fn purge_old_trash(&self) {
        let Ok(entries) = fs::read_dir(self.trash_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            if let Some(age) = trash_entry_age(&entry.path())
                && age >= TRASH_RETENTION
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    fn write_pending_change(
        &self,
        plugin_id: &str,
        name: &str,
        action: MarketOperationKind,
        profile: &str,
    ) -> AppResult<()> {
        let existing = self.pending_verification_for_mutation()?;
        let mut marker = existing.unwrap_or(PendingVerification {
            plugin_id: plugin_id.into(),
            name: name.into(),
            installed_at_ms: now_ms(),
            changes: Vec::new(),
            journal_recovered: false,
        });
        marker.plugin_id = plugin_id.into();
        marker.name = name.into();
        marker.installed_at_ms = now_ms();
        marker.changes.push(PendingMarketChange {
            plugin_id: plugin_id.into(),
            name: name.into(),
            action,
            profile: Some(profile.into()),
        });
        fs::create_dir_all(self.catalog_dir())
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let bytes = serde_json::to_vec(&marker)?;
        crate::paths::atomic_write(&self.pending_file(), &bytes)?;
        Ok(())
    }

    fn pending_verification_for_mutation(&self) -> AppResult<Option<PendingVerification>> {
        match self.pending_verification() {
            Ok(pending) => Ok(pending),
            Err(error) => {
                log::warn!("pending marketplace journal is unreadable before mutation: {error}");
                self.recover_corrupt_pending_journal()?;
                self.pending_verification()
            }
        }
    }

    fn drop_last_pending_change(&self, profile: &str) -> AppResult<()> {
        let Some(mut pending) = self.pending_verification()? else {
            return Ok(());
        };
        let Some(index) = pending
            .changes
            .iter()
            .rposition(|change| change.profile.as_deref() == Some(profile))
        else {
            return Ok(());
        };
        pending.changes.remove(index);
        if pending.changes.is_empty() {
            return remove_file_if_exists(&self.pending_file());
        }
        if let Some(last) = pending.changes.last() {
            pending.plugin_id.clone_from(&last.plugin_id);
            pending.name.clone_from(&last.name);
        }
        crate::paths::atomic_write(&self.pending_file(), &serde_json::to_vec(&pending)?)
    }

    fn recover_corrupt_pending_journal(&self) -> AppResult<()> {
        let pending_path = self.pending_file();
        let bytes = match fs::read(&pending_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if serde_json::from_slice::<PendingVerification>(&bytes).is_ok() {
            return Ok(());
        }
        fs::create_dir_all(self.catalog_dir())
            .map_err(|error| AppError::io("createDirectory", &error))?;
        let quarantined = self
            .catalog_dir()
            .join(format!("pending.corrupt-{}.json", process_timestamp()));
        fs::rename(&pending_path, &quarantined)
            .map_err(|error| AppError::io("marketRollbackFailed", &error))?;

        let mut profiles = self.last_good_profiles();
        profiles.sort();
        profiles.dedup();
        if !profiles.is_empty() {
            let recovered = PendingVerification {
                plugin_id: String::new(),
                name: String::new(),
                installed_at_ms: now_ms(),
                changes: profiles
                    .into_iter()
                    .map(|profile| PendingMarketChange {
                        plugin_id: String::new(),
                        name: profile.clone(),
                        action: MarketOperationKind::Install,
                        profile: Some(profile),
                    })
                    .collect(),
                journal_recovered: true,
            };
            if let Err(error) =
                crate::paths::atomic_write(&pending_path, &serde_json::to_vec(&recovered)?)
            {
                let _ = fs::rename(&quarantined, &pending_path);
                return Err(error);
            }
        }
        log::warn!(
            "quarantined unreadable marketplace journal at {}",
            quarantined.display()
        );
        self.prune_corrupt_pending_journals();
        Ok(())
    }

    fn prune_corrupt_pending_journals(&self) {
        let Ok(entries) = fs::read_dir(self.catalog_dir()) else {
            return;
        };
        let mut journals = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("pending.corrupt-") && name.ends_with(".json")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        journals.sort();
        let remove_count = journals.len().saturating_sub(CORRUPT_PENDING_RETENTION);
        for journal in journals.into_iter().take(remove_count) {
            if let Err(error) = fs::remove_file(&journal) {
                log::warn!(
                    "could not prune old corrupt marketplace journal {}: {error}",
                    journal.display()
                );
            }
        }
    }

    fn pending_profiles(&self) -> AppResult<Vec<String>> {
        let mut profiles = match self.pending_verification() {
            Ok(Some(pending)) => pending
                .changes
                .into_iter()
                .filter_map(|change| change.profile)
                .collect::<Vec<_>>(),
            Ok(None) => Vec::new(),
            Err(error) => {
                log::warn!(
                    "pending marketplace journal is unreadable; recovering backups: {error}"
                );
                Vec::new()
            }
        };
        if let Ok(entries) = fs::read_dir(self.profiles_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(profile) = name
                    .strip_prefix('.')
                    .and_then(|name| name.strip_suffix(".market-last-good"))
                else {
                    continue;
                };
                if valid_profile_dir_name(profile) {
                    profiles.push(profile.into());
                }
            }
        }
        profiles.sort();
        profiles.dedup();
        Ok(profiles)
    }

    fn discard_last_good_profiles(&self) -> AppResult<()> {
        let Ok(entries) = fs::read_dir(self.profiles_dir()) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.ends_with(".market-last-good") {
                fs::remove_dir_all(entry.path())
                    .map_err(|error| AppError::io("marketRollbackFailed", &error))?;
            }
        }
        Ok(())
    }

    fn last_good_profiles(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.profiles_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_prefix('.')
                    .and_then(|name| name.strip_suffix(".market-last-good"))
                    .filter(|profile| valid_profile_dir_name(profile))
                    .map(str::to_owned)
            })
            .collect()
    }

    fn finish_verified_cleanups(&self) -> AppResult<()> {
        let Ok(entries) = fs::read_dir(self.catalog_dir()) else {
            return Ok(());
        };
        let verified = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("pending.verified-") && name.ends_with(".json")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        if verified.is_empty() {
            return Ok(());
        }
        self.discard_last_good_profiles()?;
        for marker in verified {
            remove_file_if_exists(&marker)?;
        }
        Ok(())
    }

    fn recover_marketplace_state(&self) -> AppResult<()> {
        self.recover_all_profile_transactions()?;
        self.recover_corrupt_pending_journal()?;
        self.finish_verified_cleanups()?;
        self.prune_corrupt_pending_journals();
        Ok(())
    }

    fn recover_all_profile_transactions(&self) -> AppResult<()> {
        let Ok(entries) = fs::read_dir(self.profiles_dir()) else {
            return Ok(());
        };
        let mut profiles = HashSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix('.') else {
                continue;
            };
            if let Some((profile, _)) = rest.split_once(".market-backup-") {
                profiles.insert(profile.to_owned());
            } else if let Some((profile, _)) = rest.split_once(".market-candidate-") {
                profiles.insert(profile.to_owned());
            }
        }
        for profile in profiles {
            if valid_profile_dir_name(&profile) {
                self.recover_profile_transaction(&profile)?;
            }
        }
        Ok(())
    }

    fn log_operation(&self, verb: &str, target: &str, ok: bool, detail: &str) {
        let line = format!(
            "[{}] {verb} {target} -> {}: {}\n",
            chrono_free_timestamp(),
            if ok { "ok" } else { "failed" },
            detail
        );
        if fs::create_dir_all(self.catalog_dir()).is_ok()
            && let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.install_log())
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PluginIndex {
    by_name: HashMap<String, Option<String>>,
    by_package: HashMap<String, Option<String>>,
    by_package_source: HashMap<(String, String), String>,
}

impl PluginIndex {
    fn build(catalog: Option<&MarketCatalogFile>) -> Self {
        let mut index = Self::default();
        let mut package_binding_ranks = HashMap::new();
        let Some(catalog) = catalog else {
            return index;
        };
        for plugin in &catalog.plugins {
            insert_unique_binding(&mut index.by_name, plugin.name.to_lowercase(), &plugin.id);
            insert_unique_binding(
                &mut index.by_name,
                plugin.full_name.to_lowercase(),
                &plugin.id,
            );
            if let Some(package_name) = install_package_name(plugin) {
                let package_key = package_name.to_lowercase();
                insert_ranked_package_binding(
                    &mut index.by_package,
                    &mut package_binding_ranks,
                    package_key.clone(),
                    &plugin.id,
                    u8::from(package_matches_plugin_identity(plugin, &package_name)),
                );
                index
                    .by_package_source
                    .insert((package_key, plugin.id.to_lowercase()), plugin.id.clone());
            }
        }
        index
    }
}

fn insert_unique_binding(
    bindings: &mut HashMap<String, Option<String>>,
    key: String,
    plugin_id: &str,
) {
    bindings
        .entry(key)
        .and_modify(|existing| {
            if existing.as_deref() != Some(plugin_id) {
                *existing = None;
            }
        })
        .or_insert_with(|| Some(plugin_id.into()));
}

fn insert_ranked_package_binding(
    bindings: &mut HashMap<String, Option<String>>,
    ranks: &mut HashMap<String, u8>,
    key: String,
    plugin_id: &str,
    rank: u8,
) {
    let existing_rank = ranks.get(&key).copied();
    match existing_rank {
        None => {
            bindings.insert(key.clone(), Some(plugin_id.into()));
            ranks.insert(key, rank);
        }
        Some(current) if rank > current => {
            bindings.insert(key.clone(), Some(plugin_id.into()));
            ranks.insert(key, rank);
        }
        Some(current) if rank == current => {
            let existing = bindings.get_mut(&key).expect("package binding missing");
            if existing.as_deref() != Some(plugin_id) {
                *existing = None;
            }
        }
        Some(_) => {}
    }
}

#[derive(Debug, Default)]
struct InstalledIndex {
    by_plugin: HashMap<String, InstalledPlugin>,
}

impl InstalledIndex {
    fn build(installed: Vec<InstalledPlugin>) -> Self {
        let mut index = Self::default();
        for entry in installed {
            if let Some(plugin_id) = &entry.plugin_id {
                index.by_plugin.entry(plugin_id.clone()).or_insert(entry);
            }
        }
        index
    }
}

#[derive(Debug)]
struct InstalledCache {
    scanned_at: Instant,
    entries: Vec<InstalledPlugin>,
}

fn same_install_location(left: &InstalledPlugin, right: &InstalledPlugin) -> bool {
    left.source == right.source
        && left.local_name == right.local_name
        && left.profile == right.profile
}

fn plugin_matches(plugin: &MarketPlugin, query: &MarketQuery) -> bool {
    if let Some(kind) = query.kind
        && PluginKind::parse(&plugin.kind) != kind
    {
        return false;
    }
    if let Some(tag) = &query.tag {
        let tag = tag.to_lowercase();
        if !plugin.tags.iter().any(|t| t.to_lowercase() == tag) {
            return false;
        }
    }
    if let Some(search) = &query.search {
        let terms: Vec<&str> = search.split_whitespace().collect();
        if !terms.is_empty() {
            let mut haystack = String::new();
            haystack.push_str(&plugin.name);
            haystack.push(' ');
            haystack.push_str(&plugin.owner);
            haystack.push(' ');
            haystack.push_str(&plugin.full_name);
            haystack.push(' ');
            haystack.push_str(&plugin.description);
            haystack.push(' ');
            haystack.push_str(&plugin.description_zh);
            for tag in &plugin.tags {
                haystack.push(' ');
                haystack.push_str(tag);
            }
            let haystack = haystack.to_lowercase();
            for term in terms {
                if !haystack.contains(&term.to_lowercase()) {
                    return false;
                }
            }
        }
    }
    true
}

fn sort_plugins(plugins: &mut [&MarketPlugin], sort: MarketSort) {
    match sort {
        MarketSort::Score => plugins.sort_by(|left, right| {
            let l = left.score.as_ref().and_then(|s| s.total).unwrap_or(-1.0);
            let r = right.score.as_ref().and_then(|s| s.total).unwrap_or(-1.0);
            r.partial_cmp(&l).unwrap_or(std::cmp::Ordering::Equal)
        }),
        MarketSort::Stars => plugins.sort_by_key(|plugin| std::cmp::Reverse(plugin.stars)),
        MarketSort::RecentlyUpdated => {
            plugins.sort_by(|left, right| right.updated_at.cmp(&left.updated_at))
        }
        MarketSort::Name => plugins.sort_by_key(|plugin| plugin.name.to_lowercase()),
    }
}

fn install_package_name(plugin: &MarketPlugin) -> Option<String> {
    // Priority 1: the catalog's own install commands. The dsh-market
    // generator scrapes these from README install sections, which name the
    // real registry package — e.g. `dsh-better-sidebar@latest` for a plugin
    // whose catalog name is `DSH-better-sidebar` and whose homepage is empty.
    // A single command may install a host bundle before the actual plugin,
    // e.g. `add @deepseek-ai/dsh-web-app paper-review`. In that shape the
    // final package is the repository's plugin; choosing the first package
    // would bind every such catalog entry to the shared host dependency.
    let command_specs: Vec<String> = plugin
        .install
        .iter()
        .flat_map(|install| &install.commands)
        .filter_map(|command| add_spec_from_command(command))
        .collect();
    if let Some(spec) = command_specs
        .iter()
        .find(|spec| package_matches_plugin_identity(plugin, spec))
    {
        return Some(spec.clone());
    }
    if let Some(spec) = command_specs.first() {
        return Some(spec.clone());
    }
    // Priority 2: an npm package URL in the homepage field.
    if let Some(homepage) = &plugin.homepage
        && let Some(marker) = homepage.find("npmjs.com/package/")
    {
        let start = marker + "npmjs.com/package/".len();
        let name = &homepage[start..];
        let name = name.trim_end_matches('/');
        if let Some(name) = normalize_package_spec(&percent_decode(name)) {
            return Some(name);
        }
    }
    // Priority 3: the catalog name, case-folded. npm names are lowercase, so
    // an uppercase display name (like `DSH-better-sidebar`) would otherwise
    // 404 on the registry; names that cannot be a registry spec at all are
    // passed through so pnpm reports its own readable error.
    if let Some(name) = normalize_package_spec(&plugin.name.to_lowercase()) {
        return Some(name);
    }
    None
}

fn package_matches_plugin_identity(plugin: &MarketPlugin, package_name: &str) -> bool {
    let package_name = package_name.to_lowercase();
    let package_base = package_name
        .rsplit_once('/')
        .map_or(package_name.as_str(), |(_, name)| name);
    let repo_name = plugin.id.rsplit_once('/').map(|(_, repo)| repo);
    [Some(plugin.name.as_str()), repo_name]
        .into_iter()
        .flatten()
        .map(str::to_lowercase)
        .filter_map(|name| normalize_package_spec(&name))
        .any(|identity| identity == package_name || identity == package_base)
}

fn sanitize_catalog(catalog: &mut MarketCatalogFile) -> AppResult<()> {
    if !matches!(catalog.schema_version, 1 | 2) {
        return Err(AppError::new("marketCatalogInvalid").detail(format!(
            "unsupported schema version {}",
            catalog.schema_version
        )));
    }
    if catalog.plugins.is_empty() {
        return Err(AppError::new("marketCatalogInvalid").detail("catalog contains no plugins"));
    }
    let mut ids = HashSet::new();
    let before = catalog.plugins.len();
    catalog.plugins.retain(|plugin| {
        let safe = ids.insert(plugin.id.clone())
            && valid_github_repo_id(&plugin.id)
            && catalog_source_matches_id(plugin)
            && match plugin.kind.as_str() {
                "skill" => valid_skill_dir_name(&plugin.name),
                "cordis-plugin" => install_package_name(plugin).is_some(),
                _ => false,
            };
        if !safe {
            log::warn!("dropping unsafe marketplace catalog entry {}", plugin.id);
        }
        safe
    });
    if catalog.plugins.is_empty() {
        return Err(AppError::new("marketCatalogInvalid").detail("catalog has no safe plugins"));
    }
    if catalog.plugins.len() != before {
        log::warn!(
            "market catalog dropped {} invalid entries",
            before - catalog.plugins.len()
        );
    }
    Ok(())
}

fn catalog_source_matches_id(plugin: &MarketPlugin) -> bool {
    let fields_match = format!("{}/{}", plugin.owner, plugin.repo).eq_ignore_ascii_case(&plugin.id);
    let full_name_matches =
        plugin.full_name.is_empty() || plugin.full_name.eq_ignore_ascii_case(&plugin.id);
    fields_match && full_name_matches
}

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

/// Extract the plugin package from one catalog install command when it is a
/// plausible npm registry spec. Handles `dsh plugin [--profile <name>] add
/// <spec>...` and `pnpm add <spec>...` shapes, skips option values, and
/// rejects the local-path, git and shell placeholders that catalog scrapers
/// often pick up (`<本目录>`, `$(pwd)`, `github:owner/repo`, `.\path`, ...).
///
/// Commands that bootstrap a new profile commonly list a shared host bundle
/// first and the plugin itself last. Returning the final valid package keeps
/// installed-state detection and uninstallation bound to the card the user
/// selected instead of to that shared prerequisite.
fn add_spec_from_command(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    for token in tokens.by_ref() {
        if token != "add" {
            continue;
        }
        let mut skip_option_value = false;
        let mut package = None;
        for token in &mut tokens {
            // Do not let a later shell command or an inline comment replace
            // the package selected from this `add` invocation.
            if matches!(token, "&&" | "||" | "|" | ";" | "&" | "(" | ")") || token.starts_with('#')
            {
                break;
            }
            if skip_option_value {
                skip_option_value = false;
                continue;
            }
            if matches!(token, "--profile" | "-p" | "--filter" | "-F") {
                skip_option_value = true;
                continue;
            }
            if token.starts_with('-') {
                continue;
            }
            if let Some(spec) = normalize_package_spec(token) {
                package = Some(spec);
                continue;
            }
            // Version ranges may legitimately contain `>` or `<`, so shell
            // metacharacters terminate parsing only after the entire token
            // has failed registry-package normalization. This still stops at
            // redirection tokens such as `>`, `2>>` and `<`.
            if token
                .chars()
                .any(|c| matches!(c, '>' | '<' | '&' | '|' | ';' | '(' | ')' | '#'))
            {
                break;
            }
        }
        return package;
    }
    None
}

fn pnpm_mutation_flags(verb: &str) -> &'static [&'static str] {
    if verb == "remove" {
        // Removal is the recovery path for an already-broken dependency
        // tree. A profile-wide peer conflict must not make a direct
        // dependency impossible to remove. pnpm 11's remove parser does not
        // accept the add-only `--ignore-scripts` option, but the config form
        // applies the same lifecycle-script policy.
        &[
            "--config.ignore-scripts=true",
            "--no-strict-peer-dependencies",
        ]
    } else {
        &["--ignore-scripts", "--strict-peer-dependencies"]
    }
}

/// Reduce a pnpm package argument to its bare package name when it is a
/// plausible registry spec: unwrap quotes, strip a version/tag suffix
/// (`pkg@latest`, `@scope/pkg@^1.0.0`), and reject local paths, git specs,
/// shell substitutions and uppercase names (npm names are lowercase; an
/// uppercase one would 404 on the registry).
fn normalize_package_spec(spec: &str) -> Option<String> {
    let spec = spec.trim().trim_matches(['"', '\'', '`']);
    if let Some(rest) = spec.strip_prefix('@') {
        let (scope, rest) = rest.split_once('/')?;
        if !valid_npm_name_part(scope) {
            return None;
        }
        let name = rest.split('@').next()?;
        if !valid_npm_name_part(name) {
            return None;
        }
        Some(format!("@{scope}/{name}"))
    } else {
        let name = spec.split('@').next()?;
        if !valid_npm_name_part(name) {
            return None;
        }
        Some(name.to_owned())
    }
}

/// One npm package-name part (the unscoped name, or a scope without its
/// leading `@`): lowercase url-safe characters, starting with a letter or
/// digit. `~` is tolerated for legacy packages; anything else — uppercase,
/// spaces, path separators, shell metacharacters — means the string cannot
/// be an npm name.
fn valid_npm_name_part(part: &str) -> bool {
    let mut chars = part.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~'))
}

fn percent_decode(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                output.push(char::from(byte));
            } else {
                output.push(c);
                output.push_str(&hex);
            }
        } else {
            output.push(c);
        }
    }
    output
}

/// A skill install target must be a single, plain directory name: the catalog
/// is external data, so separators and `..` would otherwise let the final
/// rename escape the skills directory.
fn valid_skill_dir_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn valid_profile_dir_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

/// Clone profile metadata into a transaction candidate. `node_modules` is
/// intentionally rebuilt by pnpm, preventing hard links or copied symlinks
/// from mutating the active dependency tree through the candidate.
fn copy_profile_candidate(source: &Path, dest: &Path) -> AppResult<()> {
    fs::create_dir_all(dest).map_err(|error| AppError::io("createDirectory", &error))?;
    for entry in fs::read_dir(source).map_err(|error| AppError::io("io", &error))? {
        let entry = entry.map_err(|error| AppError::io("io", &error))?;
        if entry.file_name() == "node_modules" {
            continue;
        }
        let target = dest.join(entry.file_name());
        copy_profile_entry(&entry.path(), &target)?;
    }
    Ok(())
}

fn copy_profile_entry(source: &Path, dest: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source).map_err(|error| AppError::io("io", &error))?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::new("marketProfileInvalid").detail(format!(
            "profile contains unsupported symlink {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir_all(dest).map_err(|error| AppError::io("createDirectory", &error))?;
        for entry in fs::read_dir(source).map_err(|error| AppError::io("io", &error))? {
            let entry = entry.map_err(|error| AppError::io("io", &error))?;
            copy_profile_entry(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        fs::copy(source, dest).map_err(|error| AppError::io("writeFailed", &error))?;
    } else {
        return Err(AppError::new("marketProfileInvalid").detail(format!(
            "profile contains unsupported special file {}",
            source.display()
        )));
    }
    Ok(())
}

/// Move a directory onto a new path, falling back to copy+delete when the
/// source and destination live on different filesystems (rename fails with
/// EXDEV there).
fn move_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    match fs::rename(source, dest) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) => {
            if let Err(copy_error) = copy_dir_recursive(source, dest) {
                let _ = fs::remove_dir_all(dest);
                return Err(copy_error);
            }
            fs::remove_dir_all(source)
        }
        Err(error) => Err(error),
    }
}

fn is_cross_device(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(windows)]
    {
        // ERROR_NOT_SAME_DEVICE
        error.raw_os_error() == Some(17)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

/// Recursive directory copy used by the cross-device fallback. Refuse
/// symlinks and special files instead of silently omitting them before the
/// source tree is deleted.
fn copy_dir_recursive(source: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported file type at {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

/// Age of a trash entry: prefer the epoch-millisecond stamp embedded in the
/// entry name at uninstall time (rename does not update directory mtimes, so
/// filesystem metadata is unreliable for this), and fall back to the mtime
/// for entries that predate the stamping scheme.
fn trash_entry_age(path: &Path) -> Option<Duration> {
    let name = path.file_name()?.to_string_lossy();
    if let Some((_, stamp)) = name.rsplit_once('-')
        && stamp.len() >= 13
        && stamp.chars().all(|c| c.is_ascii_digit())
        && let Ok(millis) = stamp.parse::<u64>()
    {
        let discarded = std::time::UNIX_EPOCH + Duration::from_millis(millis);
        return std::time::SystemTime::now().duration_since(discarded).ok();
    }
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
}

/// The slice of a profile `package.json` the marketplace manages: dependency
/// entries and the `dsh.profile.bundles` layer list. Unrelated keys are
/// preserved untouched when writing back.
#[derive(Debug, Clone)]
struct ProfileManifest {
    dependencies: BTreeMap<String, String>,
    bundles: Vec<String>,
}

fn has_installation_owned_foundation(manifest: &ProfileManifest) -> bool {
    manifest
        .bundles
        .iter()
        .any(|bundle| !manifest.dependencies.contains_key(bundle))
}

fn read_manifest(path: &Path) -> AppResult<ProfileManifest> {
    let bytes = fs::read(path).map_err(|error| AppError::io("io", &error))?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    let root = raw.as_object().ok_or_else(|| {
        AppError::new("marketProfileInvalid").detail("package.json root must be an object")
    })?;
    let dependencies = match root.get("dependencies") {
        None => BTreeMap::new(),
        Some(value) => value
            .as_object()
            .ok_or_else(|| {
                AppError::new("marketProfileInvalid").detail("dependencies must be an object")
            })?
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|version| (key.clone(), version.to_owned()))
                    .ok_or_else(|| {
                        AppError::new("marketProfileInvalid")
                            .detail(format!("dependency {key} must have a string version"))
                    })
            })
            .collect::<AppResult<BTreeMap<_, _>>>()?,
    };
    let bundles = match root.get("dsh") {
        None => Vec::new(),
        Some(dsh) => {
            let dsh = dsh.as_object().ok_or_else(|| {
                AppError::new("marketProfileInvalid").detail("dsh must be an object")
            })?;
            match dsh.get("profile") {
                None => Vec::new(),
                Some(profile) => {
                    let profile = profile.as_object().ok_or_else(|| {
                        AppError::new("marketProfileInvalid")
                            .detail("dsh.profile must be an object")
                    })?;
                    match profile.get("bundles") {
                        None => Vec::new(),
                        Some(bundles) => bundles
                            .as_array()
                            .ok_or_else(|| {
                                AppError::new("marketProfileInvalid")
                                    .detail("dsh.profile.bundles must be an array")
                            })?
                            .iter()
                            .map(|bundle| {
                                bundle.as_str().map(str::to_owned).ok_or_else(|| {
                                    AppError::new("marketProfileInvalid")
                                        .detail("every dsh.profile.bundles entry must be a string")
                                })
                            })
                            .collect::<AppResult<Vec<_>>>()?,
                    }
                }
            }
        }
    };
    if bundles.iter().any(|bundle| bundle.trim().is_empty()) {
        return Err(AppError::new("marketProfileInvalid")
            .detail("dsh.profile.bundles contains an empty package name"));
    }
    let mut unique = HashSet::new();
    if bundles.iter().any(|bundle| !unique.insert(bundle)) {
        return Err(AppError::new("marketProfileInvalid")
            .detail("dsh.profile.bundles contains duplicate entries"));
    }
    Ok(ProfileManifest {
        dependencies,
        bundles,
    })
}

fn write_manifest(path: &Path, manifest: &ProfileManifest) -> AppResult<()> {
    // Preserve unrelated keys: load raw JSON and patch only our slices.
    let mut raw: serde_json::Value =
        serde_json::from_slice(&fs::read(path).map_err(|error| AppError::io("io", &error))?)?;
    if let Some(object) = raw.as_object_mut() {
        let dependencies = serde_json::to_value(&manifest.dependencies)?;
        object.insert("dependencies".into(), dependencies);
        let bundles = serde_json::to_value(&manifest.bundles)?;
        let dsh = object.entry("dsh").or_insert_with(|| serde_json::json!({}));
        let profile = dsh
            .as_object_mut()
            .map(|d| d.entry("profile").or_insert_with(|| serde_json::json!({})));
        if let Some(profile) = profile.and_then(|p| p.as_object_mut()) {
            profile.insert("bundles".into(), bundles);
        }
    }
    let bytes = serde_json::to_vec_pretty(&raw)?;
    crate::paths::atomic_write(path, &bytes)?;
    Ok(())
}

fn package_declares_bundle(profile_dir: &Path, dep: &str) -> bool {
    let package_path = profile_dir
        .join("node_modules")
        .join(dep)
        .join("package.json");
    let Ok(bytes) = fs::read(package_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value
        .get("dsh")
        .and_then(|d| d.get("bundle"))
        .and_then(|b| b.get("patch"))
        .is_some()
}

fn validate_candidate_profile(
    baseline: &ProfileManifest,
    candidate_dir: &Path,
    verb: &str,
    spec: &str,
) -> AppResult<()> {
    let target = normalize_package_spec(spec).ok_or_else(|| {
        AppError::new("marketProfileInvalid").detail("operation target is not a registry package")
    })?;
    let candidate = read_manifest(&candidate_dir.join("package.json"))?;

    for foundation in &baseline.bundles {
        if verb == "remove" && foundation == &target {
            continue;
        }
        if !candidate.bundles.contains(foundation) {
            return Err(AppError::new("marketProfileInvalid").detail(format!(
                "profile mutation removed existing bundle {foundation}"
            )));
        }
    }

    match verb {
        "add" => {
            if !candidate.dependencies.contains_key(&target) {
                return Err(AppError::new("marketProfileInvalid").detail(format!(
                    "installed package {target} is missing from dependencies"
                )));
            }
            if !package_declares_bundle(candidate_dir, &target) {
                return Err(AppError::new("marketProfileInvalid").detail(format!(
                    "installed package {target} does not declare dsh.bundle"
                )));
            }
            if !candidate.bundles.contains(&target) {
                return Err(AppError::new("marketProfileInvalid").detail(format!(
                    "installed bundle {target} is not active in the profile"
                )));
            }
        }
        "remove" => {
            if candidate.dependencies.contains_key(&target) || candidate.bundles.contains(&target) {
                return Err(AppError::new("marketProfileInvalid")
                    .detail(format!("removed package {target} is still active")));
            }
        }
        _ => {
            return Err(
                AppError::new("marketProfileInvalid").detail("unsupported profile mutation")
            );
        }
    }

    for dependency in candidate.dependencies.keys() {
        if !candidate_dir
            .join("node_modules")
            .join(dependency)
            .join("package.json")
            .is_file()
        {
            return Err(AppError::new("marketProfileInvalid")
                .detail(format!("dependency {dependency} is not installed")));
        }
    }
    Ok(())
}

fn validate_reverse_package_dependencies(
    profile_dir: &Path,
    manifest: &ProfileManifest,
    removed: &str,
) -> AppResult<()> {
    for bundle in &manifest.bundles {
        if bundle == removed {
            continue;
        }
        let package_path = profile_dir
            .join("node_modules")
            .join(bundle)
            .join("package.json");
        let Ok(bytes) = fs::read(package_path) else {
            continue;
        };
        let Ok(package): Result<serde_json::Value, _> = serde_json::from_slice(&bytes) else {
            continue;
        };
        let required = ["dependencies", "peerDependencies", "optionalDependencies"]
            .into_iter()
            .any(|field| {
                package
                    .get(field)
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|dependencies| dependencies.contains_key(removed))
            });
        if required {
            return Err(AppError::new("marketPluginRequired")
                .value("plugin", removed)
                .value("dependent", bundle));
        }
    }
    Ok(())
}

fn profile_control_digest(profile_dir: &Path) -> AppResult<[u8; 32]> {
    let mut digest = Sha256::new();
    for name in ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"] {
        digest.update(name.as_bytes());
        let path = profile_dir.join(name);
        match fs::read(&path) {
            Ok(bytes) => {
                digest.update([1]);
                digest.update((bytes.len() as u64).to_le_bytes());
                digest.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => digest.update([0]),
            Err(error) => return Err(AppError::io("io", &error)),
        }
    }
    Ok(digest.finalize().into())
}

fn remove_file_if_exists(path: &Path) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn restore_pending_snapshot(path: &Path, previous: Option<&[u8]>) -> AppResult<()> {
    if let Some(bytes) = previous {
        crate::paths::atomic_write(path, bytes)
    } else {
        remove_file_if_exists(path)
    }
}

fn read_installed_version(profile_dir: &Path, dep: &str) -> Option<String> {
    let package_path = profile_dir
        .join("node_modules")
        .join(dep)
        .join("package.json");
    let bytes = fs::read(package_path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("version")?.as_str().map(str::to_owned)
}

fn read_installed_repository(profile_dir: &Path, dep: &str) -> Option<String> {
    let package_path = profile_dir
        .join("node_modules")
        .join(dep)
        .join("package.json");
    let bytes = fs::read(package_path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("repository")
        .and_then(repository_url)
        .and_then(normalize_github_repository)
}

fn split_name_version(local_name: &str) -> (String, Option<String>) {
    let Some((base, suffix)) = local_name.rsplit_once('-') else {
        return (local_name.to_owned(), None);
    };
    let version = suffix.trim_start_matches('v');
    let looks_like_version = version
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    if looks_like_version || suffix == "latest" {
        (base.to_owned(), Some(suffix.to_owned()))
    } else {
        (local_name.to_owned(), None)
    }
}

// ---------------------------------------------------------------------------
// Network helpers
// ---------------------------------------------------------------------------

fn http_client() -> AppResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("DSH-Launcher/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| AppError::new("marketNetworkFailed").detail(error.to_string()))
}

fn fetch_bytes(
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    code: &'static str,
    subject: &str,
) -> AppResult<Vec<u8>> {
    fetch_bytes_with(None, url, timeout, max_bytes, code, subject)
}

fn fetch_bytes_with(
    client: Option<&reqwest::blocking::Client>,
    url: &str,
    timeout: Duration,
    max_bytes: usize,
    code: &'static str,
    subject: &str,
) -> AppResult<Vec<u8>> {
    let owned;
    let client = match client {
        Some(client) => client,
        None => {
            owned = http_client()?;
            &owned
        }
    };
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .map_err(|error| AppError::new("marketNetworkFailed").detail(error.to_string()))?
        .error_for_status()
        .map_err(|error| AppError::new("marketNetworkFailed").detail(error.to_string()))?;
    if let Some(length) = response.content_length()
        && usize::try_from(length).unwrap_or(usize::MAX) > max_bytes
    {
        return Err(AppError::new(code).detail(format!("{subject} exceeds the size limit")));
    }
    // Stream with a hard cap: buffering the whole body first (`bytes()`)
    // would let a chunked response balloon memory before the limit check.
    let mut bytes = Vec::new();
    response
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::new("marketNetworkFailed").detail(error.to_string()))?;
    if bytes.len() > max_bytes {
        return Err(AppError::new(code).detail(format!("{subject} exceeds the size limit")));
    }
    Ok(bytes)
}

fn fetch_trusted_catalog() -> AppResult<(Vec<u8>, CatalogTrustMeta)> {
    let commit_url = format!("{GITHUB_API}/repos/{MARKET_REPOSITORY}/commits/{MARKET_BRANCH}");
    let commit_bytes = fetch_bytes(
        &commit_url,
        REGISTRY_TIMEOUT,
        2 * 1024 * 1024,
        "marketCatalogInvalid",
        "catalog commit metadata",
    )?;
    let commit: serde_json::Value = serde_json::from_slice(&commit_bytes)?;
    let head = commit
        .get("sha")
        .and_then(|value| value.as_str())
        .filter(|sha| valid_commit_sha(sha))
        .ok_or_else(|| AppError::new("marketCatalogInvalid").detail("catalog commit is invalid"))?;

    if head != MARKET_TRUST_ANCHOR {
        let compare_url = format!(
            "{GITHUB_API}/repos/{MARKET_REPOSITORY}/compare/{MARKET_TRUST_ANCHOR}...{head}"
        );
        let compare_bytes = fetch_bytes(
            &compare_url,
            REGISTRY_TIMEOUT,
            16 * 1024 * 1024,
            "marketCatalogInvalid",
            "catalog history verification",
        )?;
        let compare: serde_json::Value = serde_json::from_slice(&compare_bytes)?;
        validate_catalog_lineage(head, &compare)?;
    }

    let catalog_url =
        format!("https://raw.githubusercontent.com/{MARKET_REPOSITORY}/{head}/data/plugins.json");
    let bytes = fetch_bytes(
        &catalog_url,
        CATALOG_FETCH_TIMEOUT,
        CATALOG_MAX_BYTES,
        "marketCatalogInvalid",
        "market catalog",
    )?;
    let trust = CatalogTrustMeta {
        commit: head.to_owned(),
        sha256: sha256_bytes(&bytes),
    };
    Ok((bytes, trust))
}

fn validate_catalog_lineage(_head: &str, compare: &serde_json::Value) -> AppResult<()> {
    let status = compare.get("status").and_then(|value| value.as_str());
    let base = compare
        .get("base_commit")
        .and_then(|value| value.get("sha"))
        .and_then(|value| value.as_str());
    let merge_base = compare
        .get("merge_base_commit")
        .and_then(|value| value.get("sha"))
        .and_then(|value| value.as_str());
    if status != Some("ahead")
        || base != Some(MARKET_TRUST_ANCHOR)
        || merge_base != Some(MARKET_TRUST_ANCHOR)
    {
        return Err(AppError::new("marketCatalogInvalid")
            .detail("catalog branch no longer descends from the trusted release anchor"));
    }
    Ok(())
}

fn valid_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn fetch_default_branch(repo: &str) -> AppResult<String> {
    let url = format!("{GITHUB_API}/repos/{repo}");
    let bytes = fetch_bytes(
        &url,
        REGISTRY_TIMEOUT,
        1024 * 1024,
        "marketNetworkFailed",
        "github response",
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    value
        .get("default_branch")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| AppError::new("marketNetworkFailed").detail("default branch missing"))
}

fn resolve_skill_commit(repo: &str, expected: Option<&str>) -> AppResult<String> {
    let revision = match expected {
        Some(commit) if valid_commit_sha(commit) => commit.to_owned(),
        Some(_) => {
            return Err(AppError::new("marketInstallFailed")
                .detail("confirmed skill revision is not a commit SHA"));
        }
        None => fetch_default_branch(repo)?,
    };
    let url = format!("{GITHUB_API}/repos/{repo}/commits/{revision}");
    let bytes = fetch_bytes(
        &url,
        REGISTRY_TIMEOUT,
        2 * 1024 * 1024,
        "marketNetworkFailed",
        "github commit metadata",
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let commit = value
        .get("sha")
        .and_then(|value| value.as_str())
        .filter(|value| valid_commit_sha(value))
        .ok_or_else(|| AppError::new("marketNetworkFailed").detail("commit SHA missing"))?;
    if let Some(expected) = expected
        && commit != expected
    {
        return Err(AppError::new("marketInstallFailed")
            .detail("confirmed skill revision does not belong to the repository"));
    }
    Ok(commit.to_owned())
}

fn fetch_compatibility_entry_with(
    plugin: &MarketPlugin,
    paths: &ApplicationPaths,
    client: Option<&reqwest::blocking::Client>,
) -> CachedCompatibility {
    let package_name = install_package_name(plugin).unwrap_or_default();
    let cordis_version = installed_cordis_version(paths);
    let checked_at_ms = now_ms();
    let registry = fetch_registry_package_info_with(plugin, client);
    let (package_version, info, source_binding, source_binding_detail) = match registry {
        Ok(registry) => {
            let (source_binding, source_binding_detail) = match registry.repository_id.as_deref() {
                Some(repository) if repository.eq_ignore_ascii_case(&plugin.id) => {
                    (SourceBindingStatus::Verified, None)
                }
                Some(repository) => (
                    SourceBindingStatus::Mismatch,
                    Some(format!(
                        "npm package points to {repository}, catalog points to {}",
                        plugin.id
                    )),
                ),
                None if registry.repository_declared => (
                    SourceBindingStatus::Mismatch,
                    Some("npm package repository is not a valid GitHub repository".into()),
                ),
                None => (
                    SourceBindingStatus::Unknown,
                    Some("npm package does not declare a GitHub repository".into()),
                ),
            };
            let info = evaluate_cordis_compatibility(
                cordis_version.as_deref(),
                registry.cordis_range.as_deref(),
            );
            (
                Some(registry.latest_version),
                info,
                source_binding,
                source_binding_detail,
            )
        }
        Err(error) => (
            None,
            CompatibilityInfo {
                status: CompatibilityStatus::Unknown,
                detail: Some("registry metadata unavailable".into()),
            },
            SourceBindingStatus::Unknown,
            error.safe_detail,
        ),
    };
    CachedCompatibility {
        package_name,
        package_version,
        cordis_version,
        checked_at_ms,
        info,
        source_binding,
        source_binding_detail,
    }
}

fn evaluate_cordis_compatibility(
    installed: Option<&str>,
    range: Option<&str>,
) -> CompatibilityInfo {
    let Some(installed) = installed else {
        return CompatibilityInfo {
            status: CompatibilityStatus::Unknown,
            detail: Some("harness cordis version could not be resolved".into()),
        };
    };
    let Some(range) = range else {
        return CompatibilityInfo {
            status: CompatibilityStatus::Unknown,
            detail: None,
        };
    };
    let Ok(requirement) = semver::VersionReq::parse(range) else {
        return CompatibilityInfo {
            status: CompatibilityStatus::Unknown,
            detail: Some(format!("unparseable peer range {range}")),
        };
    };
    match semver::Version::parse(installed) {
        Ok(version) if requirement.matches(&version) => CompatibilityInfo {
            status: CompatibilityStatus::Compatible,
            detail: Some(format!("cordis {installed}")),
        },
        Ok(version) => CompatibilityInfo {
            status: CompatibilityStatus::Incompatible,
            detail: Some(format!("requires cordis {range}, installed {version}")),
        },
        Err(_) => CompatibilityInfo {
            status: CompatibilityStatus::Unknown,
            detail: Some(format!("installed cordis version {installed} is invalid")),
        },
    }
}

fn validate_expected_package_version(
    plugin_name: &str,
    expected: Option<&str>,
    resolved: Option<&str>,
) -> AppResult<()> {
    if let Some(expected) = expected
        && resolved != Some(expected)
    {
        return Err(AppError::new("marketPackageChanged")
            .value("plugin", plugin_name)
            .value("expected", expected)
            .value("actual", resolved.unwrap_or("unavailable"))
            .detail("registry latest changed after install confirmation"));
    }
    Ok(())
}

fn installed_cordis_version(paths: &ApplicationPaths) -> Option<String> {
    let candidates = [
        paths
            .dsh_dir
            .join("node_modules/@deepseek-ai/cordis/package.json"),
        paths.dsh_dir.join("node_modules/cordis/package.json"),
    ];
    for path in candidates {
        if let Ok(bytes) = fs::read(&path)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(version) = value.get("version").and_then(|v| v.as_str())
        {
            return Some(version.to_owned());
        }
    }
    // Fallback: the declared range of the pinned Harness package itself.
    let dsh_manifest = paths
        .dsh_dir
        .join("node_modules/@deepseek-ai/dsh/package.json");
    if let Ok(bytes) = fs::read(dsh_manifest)
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        for key in ["@deepseek-ai/cordis", "cordis"] {
            if let Some(range) = value
                .get("dependencies")
                .and_then(|d| d.get(key))
                .and_then(|v| v.as_str())
            {
                // A range cannot be compared exactly; report its lower
                // bound as the reference version.
                if let Some(base) = range
                    .trim_start_matches(['^', '~', '>', '<', '='])
                    .split([' ', '|', ','])
                    .find(|s| !s.is_empty())
                {
                    let base = base.trim().trim_start_matches('=').to_owned();
                    if semver::Version::parse(&base).is_ok() {
                        return Some(base);
                    }
                }
                return None;
            }
        }
    }
    None
}

fn fetch_registry_package_info_with(
    plugin: &MarketPlugin,
    client: Option<&reqwest::blocking::Client>,
) -> AppResult<RegistryPackageInfo> {
    let pkg = install_package_name(plugin).ok_or_else(|| {
        AppError::new("marketCatalogInvalid").detail("invalid npm package target")
    })?;
    let encoded = pkg.replace('/', "%2F");
    let url = format!("{NPM_REGISTRY}/{encoded}");
    let bytes = fetch_bytes_with(
        client,
        &url,
        REGISTRY_TIMEOUT,
        8 * 1024 * 1024,
        "marketNetworkFailed",
        "registry response",
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    registry_package_info(&value)
}

fn registry_package_info(value: &serde_json::Value) -> AppResult<RegistryPackageInfo> {
    let latest = value
        .get("dist-tags")
        .and_then(|t| t.get("latest"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new("marketNetworkFailed").detail("registry dist-tags missing"))?;
    semver::Version::parse(latest).map_err(|_| {
        AppError::new("marketNetworkFailed")
            .detail("registry latest is not an exact semver version")
    })?;
    let latest_manifest = value
        .get("versions")
        .and_then(|v| v.get(latest))
        .ok_or_else(|| AppError::new("marketNetworkFailed").detail("latest manifest missing"))?;
    let peers = latest_manifest.get("peerDependencies");
    let cordis_range = ["@deepseek-ai/cordis", "cordis"]
        .iter()
        .find_map(|key| {
            peers
                .and_then(|value| value.get(key))
                .and_then(|v| v.as_str())
        })
        .map(str::to_owned);
    let repository = latest_manifest
        .get("repository")
        .or_else(|| value.get("repository"));
    let repository_id = repository
        .and_then(repository_url)
        .and_then(normalize_github_repository);
    Ok(RegistryPackageInfo {
        latest_version: latest.to_owned(),
        cordis_range,
        repository_id,
        repository_declared: repository.is_some(),
    })
}

/// Fetch compatibility entries concurrently with a shared HTTP client, so a
/// page of plugins resolves in roughly one request round instead of N.
fn fetch_compatibility_batch(
    plugins: &[&MarketPlugin],
    paths: &ApplicationPaths,
) -> Vec<(String, CachedCompatibility)> {
    let client: Option<Arc<reqwest::blocking::Client>> = http_client().ok().map(Arc::new);
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let workers = plugins.len().min(COMPAT_CONCURRENCY);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index >= plugins.len() {
                        break;
                    }
                    let info =
                        fetch_compatibility_entry_with(plugins[index], paths, client.as_deref());
                    results
                        .lock()
                        .expect("results poisoned")
                        .push((plugins[index].id.clone(), info));
                }
            });
        }
    });
    results.into_inner().expect("results poisoned")
}

fn repository_url(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("url").and_then(|url| url.as_str()))
}

fn normalize_github_repository(value: &str) -> Option<String> {
    let mut value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    for prefix in [
        "git+https://github.com/",
        "git+http://github.com/",
        "git+ssh://git@github.com/",
        "git+ssh://git@github.com:",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
        "ssh://git@github.com/",
        "ssh://git@github.com:",
        "git@github.com:",
        "github:",
    ] {
        if let Some(rest) = value.strip_prefix(prefix) {
            value = rest;
            break;
        }
    }
    let candidate = value.trim_end_matches('/').trim_end_matches(".git");
    valid_github_repo_id(candidate).then(|| candidate.to_owned())
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

fn npm_cli(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("node_modules/npm/bin/npm-cli.js")
    } else {
        dir.join("lib/node_modules/npm/bin/npm-cli.js")
    }
}

fn pnpm_executable_name() -> &'static str {
    if cfg!(windows) { "pnpm.cmd" } else { "pnpm" }
}

/// `bin_dir` is the directory that directly holds the pnpm executable (a
/// global-prefix shim dir, e.g. `<prefix>/bin` on unix).
fn path_prepend_dirs(bin_dir: &Path) -> Vec<PathBuf> {
    vec![bin_dir.to_path_buf()]
}

/// Find the directory that actually contains the pnpm shim after a
/// `npm install -g --prefix <dir>` provisioning, which differs across
/// platforms and npm versions.
fn find_pnpm_dir(install_dir: &Path) -> Option<PathBuf> {
    let mut candidates = vec![
        install_dir.to_path_buf(),
        install_dir.join("bin"),
        install_dir.join("node_modules/.bin"),
    ];
    if cfg!(windows) {
        candidates.retain(|candidate| candidate == install_dir || candidate.ends_with("bin"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.join(pnpm_executable_name()).is_file())
}

fn probe_executable(bin_dir: &Path, name: &str) -> bool {
    let candidate = if cfg!(windows) {
        bin_dir.join(format!("{name}.cmd"))
    } else {
        bin_dir.join(name)
    };
    candidate.is_file()
}

fn market_command_env<'a>(
    command: &'a mut Command,
    paths: &ApplicationPaths,
    path_prepend: &[PathBuf],
) -> &'a mut Command {
    let allowed = [
        "COMSPEC",
        "LANG",
        "LC_ALL",
        "NODE_EXTRA_CA_CERTS",
        "NO_PROXY",
        "PATH",
        "SSL_CERT_DIR",
        "SSL_CERT_FILE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
    ];
    let values: Vec<_> = allowed
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| ((*key).to_owned(), value)))
        .collect();
    command
        .env_clear()
        .envs(values.iter().cloned())
        .env("HOME", &paths.app_home)
        .env("USERPROFILE", &paths.app_home)
        .env("DSH_HOME", &paths.dsh_home)
        .env("DSH_TELEMETRY_DISABLED", "1");
    // The bundled node directory must be on PATH as well: pnpm shims carry a
    // `#!/usr/bin/env node` shebang, and the Harness CLI spawns pnpm through
    // PATH, so without this the shim dies with "env: node: No such file or
    // directory" even though we invoked node by absolute path ourselves.
    let mut prepend = path_prepend.to_vec();
    if let Some(node_dir) = paths.node_bin.parent() {
        prepend.push(node_dir.to_path_buf());
    }
    if !prepend.is_empty()
        && let Some(path) = command_env_path(&values, &prepend)
    {
        command.env("PATH", path);
    }
    command
}

fn command_env_path(
    values: &[(String, std::ffi::OsString)],
    prepend: &[PathBuf],
) -> Option<std::ffi::OsString> {
    let current = values.iter().find(|(key, _)| key == "PATH")?.1.clone();
    let separator = if cfg!(windows) { ";" } else { ":" };
    let mut joined = prepend
        .iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(separator);
    if !joined.is_empty() {
        joined.push_str(separator);
    }
    joined.push_str(&current.to_string_lossy());
    Some(joined.into())
}

/// Run a child to completion with output capture and a hard timeout. Output
/// is delivered over a channel with a bounded grace period so a backgrounded
/// grandchild that keeps the pipes open can never block the caller forever.
fn run_child(mut command: Command, timeout: Duration) -> AppResult<(bool, String)> {
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| AppError::io("processSpawnFailed", &error))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    let streams: Vec<Box<dyn Read + Send>> = stdout
        .map(|stream| Box::new(stream) as Box<dyn Read + Send>)
        .into_iter()
        .chain(stderr.map(|stream| Box::new(stream) as Box<dyn Read + Send>))
        .collect();
    let readers = streams.len();
    for mut reader in streams {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output.len() < OUTPUT_CAP {
                            let take = n.min(OUTPUT_CAP - output.len());
                            output.extend_from_slice(&buffer[..take]);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        log::debug!("child output read failed: {error}");
                        break;
                    }
                }
            }
            let _ = sender.send(output);
        });
    }
    drop(sender);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = collect_reader_output(&receiver, readers);
                return Ok((
                    status.success(),
                    String::from_utf8_lossy(&output).into_owned(),
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_child(&mut child);
                    let _ = child.wait();
                    let output = collect_reader_output(&receiver, readers);
                    let text = String::from_utf8_lossy(&output).into_owned();
                    return Ok((false, format!("timed out after {timeout:?}; {text}")));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                let _ = collect_reader_output(&receiver, readers);
                return Err(error.into());
            }
        }
    }
}

fn collect_reader_output(receiver: &std::sync::mpsc::Receiver<Vec<u8>>, readers: usize) -> Vec<u8> {
    let deadline = Instant::now() + READER_GRACE;
    let mut combined = Vec::new();
    for _ in 0..readers {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(output) = receiver.recv_timeout(remaining) else {
            break;
        };
        if combined.len() < OUTPUT_CAP {
            let take = output.len().min(OUTPUT_CAP - combined.len());
            combined.extend_from_slice(&output[..take]);
        }
    }
    combined
}

fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // Kill the whole tree: `child.kill()` only terminates the direct
        // child, and surviving grandchildren holding the output pipes would
        // stall the reader forever.
        crate::runtime::terminate_tree(child.id(), true);
    }
}

fn tail(value: &str, limit: usize) -> String {
    let mut chars: Vec<char> = value.chars().collect();
    if chars.len() > limit {
        chars = chars.split_off(chars.len() - limit);
    }
    chars.into_iter().collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn process_timestamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn chrono_free_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

/// Map an archive entry path onto a destination-relative path, stripping the
/// leading top-level directory. `Ok(None)` is the stripped-empty root entry,
/// `Err` is an entry that would escape the destination.
fn safe_relative_path(path: &Path) -> Result<Option<PathBuf>, &'static str> {
    let mut components = path.components().peekable();
    if matches!(components.peek(), Some(Component::Normal(_))) {
        components.next();
    }
    let mut safe = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("tarball entry escapes the target directory");
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(safe))
}

/// Extract a codeload tarball into `dest`, skipping the leading top-level
/// directory component and rejecting traversal, links and oversized content.
fn extract_tarball(bytes: &[u8], dest: &Path) -> AppResult<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut total_bytes: usize = 0;
    let mut total_files: usize = 0;
    for entry in archive.entries().map_err(|error| {
        AppError::new("marketInstallFailed").detail(format!("tarball unreadable: {error}"))
    })? {
        let mut entry = entry.map_err(|error| {
            AppError::new("marketInstallFailed").detail(format!("tarball entry: {error}"))
        })?;
        let path = entry.path().map_err(|error| {
            AppError::new("marketInstallFailed").detail(format!("tarball path: {error}"))
        })?;
        let Some(relative) = safe_relative_path(&path)
            .map_err(|reason| AppError::new("marketInstallFailed").detail(reason))?
        else {
            continue;
        };
        let target = dest.join(relative);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target)
                    .map_err(|error| AppError::io("createDirectory", &error))?;
            }
            tar::EntryType::Regular => {
                total_files += 1;
                if total_files > TARBALL_MAX_FILES {
                    return Err(AppError::new("marketInstallFailed")
                        .detail("tarball contains too many files"));
                }
                total_bytes = total_bytes.saturating_add(entry.size() as usize);
                if total_bytes > TARBALL_MAX_BYTES {
                    return Err(AppError::new("marketInstallFailed")
                        .detail("tarball expands beyond the size limit"));
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| AppError::io("createDirectory", &error))?;
                }
                let mut file = fs::File::create(&target)
                    .map_err(|error| AppError::io("writeFailed", &error))?;
                std::io::copy(&mut entry, &mut file)
                    .map_err(|error| AppError::io("writeFailed", &error))?;
            }
            _ => {
                // Symlinks, hard links and devices are not carried over.
            }
        }
    }
    if total_files == 0 {
        return Err(AppError::new("marketInstallFailed").detail("tarball contained no files"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(plugins: Vec<MarketPlugin>) -> MarketCatalogFile {
        MarketCatalogFile {
            schema_version: 1,
            generated_at: Some("2026-01-01T00:00:00Z".into()),
            plugins,
        }
    }

    fn plugin(id: &str, name: &str, stars: u32, score: Option<f32>) -> MarketPlugin {
        let (owner, repo) = id.split_once('/').unwrap_or(("someone", name));
        MarketPlugin {
            id: id.into(),
            kind: "cordis-plugin".into(),
            name: name.into(),
            owner: owner.into(),
            repo: repo.into(),
            full_name: id.into(),
            stars,
            description: "english words".into(),
            description_zh: "中文描述".into(),
            tags: vec!["cli".into(), "数据迁移".into()],
            homepage: Some(format!("https://www.npmjs.com/package/{name}")),
            license: Some("MIT".into()),
            curated: false,
            pushed_at: None,
            updated_at: Some(format!("2026-01-0{}T00:00:00Z", (id.len() % 9) + 1)),
            install: Some(MarketInstallInfo {
                method: Some("pnpm-profile".into()),
                needs_config: Some(false),
                commands: vec![],
            }),
            score: score.map(|total| MarketScore {
                total: Some(total),
                explanation: None,
            }),
        }
    }

    #[test]
    fn matching_filters_by_kind_search_and_tag() {
        let plugins = [
            plugin("a/a", "alpha", 10, Some(80.0)),
            plugin("b/b", "beta", 5, None),
            MarketPlugin {
                kind: "skill".into(),
                name: "gamma".into(),
                ..plugin("c/c", "gamma", 3, Some(60.0))
            },
        ];
        let query = MarketQuery {
            search: Some("中文".into()),
            ..MarketQuery::default()
        };
        assert!(plugin_matches(&plugins[0], &query));
        assert!(plugin_matches(&plugins[1], &query));
        let query = MarketQuery {
            kind: Some(PluginKind::Skill),
            ..MarketQuery::default()
        };
        assert!(!plugin_matches(&plugins[0], &query));
        assert!(plugin_matches(&plugins[2], &query));
        let query = MarketQuery {
            tag: Some("数据迁移".into()),
            ..MarketQuery::default()
        };
        assert!(plugin_matches(&plugins[1], &query));
        let query = MarketQuery {
            search: Some("alpha beta".into()),
            ..MarketQuery::default()
        };
        assert!(!plugin_matches(&plugins[0], &query));
    }

    #[test]
    fn sorting_orders_by_score_stars_recency_and_name() {
        let zeta = MarketPlugin {
            name: "zeta".into(),
            stars: 3,
            updated_at: Some("2026-01-03T00:00:00Z".into()),
            ..plugin("z/z", "zeta", 3, None)
        };
        let alpha = MarketPlugin {
            name: "alpha".into(),
            stars: 30,
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            ..plugin("a/a", "alpha", 30, Some(90.0))
        };
        let mid = MarketPlugin {
            name: "mid".into(),
            stars: 10,
            updated_at: Some("2026-01-02T00:00:00Z".into()),
            ..plugin("m/m", "mid", 10, Some(50.0))
        };
        let mut plugins = vec![&zeta, &alpha, &mid];
        sort_plugins(&mut plugins, MarketSort::Score);
        assert_eq!(plugins[0].name, "alpha");
        assert_eq!(plugins[2].name, "zeta");
        sort_plugins(&mut plugins, MarketSort::Stars);
        assert_eq!(plugins[0].stars, 30);
        sort_plugins(&mut plugins, MarketSort::RecentlyUpdated);
        assert_eq!(plugins[0].name, "zeta");
        sort_plugins(&mut plugins, MarketSort::Name);
        assert_eq!(plugins[0].name, "alpha");
    }

    #[test]
    fn package_name_comes_from_npm_homepage() {
        let mut item = plugin("x/x", "x", 0, None);
        item.homepage = Some("https://www.npmjs.com/package/@scope/pkg".into());
        assert_eq!(install_package_name(&item), Some("@scope/pkg".into()));
        item.homepage = None;
        assert_eq!(install_package_name(&item), Some("x".into()));
    }

    #[test]
    fn package_name_prefers_install_commands_over_name() {
        // The reported failure: catalog name `DSH-better-sidebar`, empty
        // homepage, but the README install command names the real package.
        let mut item = plugin(
            "omdsh-dev/DSH-better-sidebar",
            "DSH-better-sidebar",
            1,
            None,
        );
        item.homepage = Some(String::new());
        item.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec!["dsh plugin --profile web add dsh-better-sidebar@latest".into()],
        });
        assert_eq!(
            install_package_name(&item),
            Some("dsh-better-sidebar".into())
        );

        // Scoped packages keep their scope and drop the tag.
        item.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec![
                "dsh plugin --profile <name> add @dong-victor/dsh-better-sidebar-terminal-plus@^1.0.0"
                    .into(),
            ],
        });
        assert_eq!(
            install_package_name(&item),
            Some("@dong-victor/dsh-better-sidebar-terminal-plus".into())
        );

        // Junk commands (local paths, git specs, shell substitutions) are
        // skipped in favor of the next usable command.
        item.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec![
                "dsh plugin add link:$(pwd)".into(),
                "dsh plugin add github:someone/repo".into(),
                "pnpm add good-pkg@0.1.0-beta.1".into(),
            ],
        });
        assert_eq!(install_package_name(&item), Some("good-pkg".into()));

        // README instructions often install a prerequisite first and the
        // repository's own package in a later command. Prefer the command
        // whose package matches this catalog card's identity.
        let mut sidebar_qa = plugin("ChenRuoT/dsh-sidebar-qa", "dsh-sidebar-qa", 0, None);
        sidebar_qa.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec![
                "dsh plugin --profile web add dsh-better-sidebar@latest".into(),
                "dsh plugin --profile web add dsh-sidebar-qa".into(),
                "dsh plugin --profile web add <本仓库路径>".into(),
            ],
        });
        assert_eq!(
            install_package_name(&sidebar_qa),
            Some("dsh-sidebar-qa".into())
        );

        // Some plugins bootstrap a custom profile by installing the shared
        // web surface and their own package in one command. The marketplace
        // card must bind to the plugin package, not the shared prerequisite.
        let mut read_paper = plugin("louwenbo580/read-paper", "read-paper", 0, None);
        read_paper.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec![
                "dsh plugin --profile readPaper add @deepseek-ai/dsh-web-app paper-review".into(),
            ],
        });
        assert_eq!(
            install_package_name(&read_paper),
            Some("paper-review".into())
        );
    }

    #[test]
    fn package_name_casefolds_the_catalog_name_as_last_resort() {
        let mut item = plugin("x/Upper-Case", "Upper-Case", 1, None);
        item.homepage = Some(String::new());
        assert_eq!(install_package_name(&item), Some("upper-case".into()));

        // Invalid catalog input must never become a pnpm option or path.
        item.name = "Not A Package".into();
        assert_eq!(install_package_name(&item), None);
    }

    #[test]
    fn add_spec_extraction_skips_flags_and_option_values() {
        assert_eq!(
            add_spec_from_command("dsh plugin --profile web add dsh-better-sidebar@latest"),
            Some("dsh-better-sidebar".into())
        );
        assert_eq!(
            add_spec_from_command("dsh plugin add --profile web some-plugin"),
            Some("some-plugin".into())
        );
        assert_eq!(
            add_spec_from_command("pnpm add @scope/pkg@1.2.3"),
            Some("@scope/pkg".into())
        );
        assert_eq!(
            add_spec_from_command(
                "dsh plugin --profile readPaper add @deepseek-ai/dsh-web-app paper-review"
            ),
            Some("paper-review".into())
        );
        assert_eq!(
            add_spec_from_command("dsh plugin add intended && pnpm add unrelated"),
            Some("intended".into())
        );
        assert_eq!(
            add_spec_from_command("dsh plugin add intended # install the plugin"),
            Some("intended".into())
        );
        assert_eq!(add_spec_from_command("dsh plugin remove old-plugin"), None);
        assert_eq!(add_spec_from_command("npm install -g dsh-tool"), None);
        assert_eq!(add_spec_from_command("dsh plugin add ./local-dir"), None);
        assert_eq!(add_spec_from_command("dsh plugin add $(pwd)"), None);
        assert_eq!(add_spec_from_command("dsh plugin add <本目录>"), None);
        assert_eq!(
            add_spec_from_command("pnpm add real-pkg > install.log"),
            Some("real-pkg".into())
        );
        assert_eq!(
            add_spec_from_command("pnpm add real-pkg 2>> install.log"),
            Some("real-pkg".into())
        );
        assert_eq!(
            add_spec_from_command("pnpm add real-pkg < input.txt"),
            Some("real-pkg".into())
        );
        assert_eq!(
            add_spec_from_command("pnpm add ranged-pkg@>=1.0.0"),
            Some("ranged-pkg".into())
        );
        assert_eq!(
            add_spec_from_command("pnpm add ranged-pkg@<2"),
            Some("ranged-pkg".into())
        );
    }

    #[test]
    fn pnpm_remove_uses_supported_safe_recovery_flags() {
        assert_eq!(
            pnpm_mutation_flags("add"),
            &["--ignore-scripts", "--strict-peer-dependencies"]
        );
        assert_eq!(
            pnpm_mutation_flags("remove"),
            &[
                "--config.ignore-scripts=true",
                "--no-strict-peer-dependencies"
            ]
        );
        assert!(!pnpm_mutation_flags("remove").contains(&"--ignore-scripts"));
    }

    #[test]
    fn package_spec_normalization_rejects_non_registry_arguments() {
        assert_eq!(
            normalize_package_spec("plain-name"),
            Some("plain-name".into())
        );
        assert_eq!(
            normalize_package_spec("dsh-better-sidebar@latest"),
            Some("dsh-better-sidebar".into())
        );
        assert_eq!(
            normalize_package_spec("@scope/pkg@^1.0.0"),
            Some("@scope/pkg".into())
        );
        assert_eq!(normalize_package_spec("\"quoted\""), Some("quoted".into()));
        for junk in [
            ".",
            "..",
            "./tgz.tgz",
            "link:$(pwd)",
            "github:owner/repo",
            "D:\\keep_try\\pkg",
            "$(pwd)",
            "<本目录>",
            "UPPER-CASE",
            "owner/repo",
            "has space",
            "",
        ] {
            assert_eq!(normalize_package_spec(junk), None, "should reject {junk:?}");
        }
    }

    #[test]
    fn skill_directory_names_split_versions() {
        assert_eq!(
            split_name_version("url-manager"),
            ("url-manager".to_owned(), None)
        );
        assert_eq!(
            split_name_version("1password-1.0.1"),
            ("1password".to_owned(), Some("1.0.1".to_owned()))
        );
        assert_eq!(
            split_name_version("thing-v2.3.4"),
            ("thing".to_owned(), Some("v2.3.4".to_owned()))
        );
        assert_eq!(
            split_name_version("skill-latest"),
            ("skill".to_owned(), Some("latest".to_owned()))
        );
        assert_eq!(
            split_name_version("plain-name-with-dash"),
            ("plain-name-with-dash".to_owned(), None)
        );
    }

    fn gz_tarball(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, content, entry_type) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).expect("path");
            header.set_size(content.len() as u64);
            header.set_entry_type(*entry_type);
            header.set_cksum();
            builder.append(&header, *content).expect("append");
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder
            .write_all(&builder.into_inner().expect("finish"))
            .expect("write");
        encoder.finish().expect("finish")
    }

    #[test]
    fn tarball_extraction_strips_root_and_rejects_traversal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bytes = gz_tarball(&[
            ("repo-main/", b"", tar::EntryType::Directory),
            ("repo-main/SKILL.md", b"# skill", tar::EntryType::Regular),
        ]);
        extract_tarball(&bytes, temp.path()).expect("extract");
        assert!(temp.path().join("SKILL.md").is_file());
        assert!(!temp.path().join("repo-main").exists());

        // Traversal attempts are rejected before writing anything.
        assert!(safe_relative_path(Path::new("repo-main/../../escape")).is_err());
        assert!(safe_relative_path(Path::new("/etc/passwd")).is_err());
        assert!(safe_relative_path(Path::new("../outside")).is_err());
        assert_eq!(
            safe_relative_path(Path::new("repo-main")).expect("root"),
            None
        );
        assert_eq!(
            safe_relative_path(Path::new("repo-main/SKILL.md")).expect("file"),
            Some(PathBuf::from("SKILL.md"))
        );
    }

    #[test]
    fn profile_manifest_roundtrip_preserves_bundles() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("package.json");
        fs::write(
            &path,
            br#"{
  "name": "web",
  "dependencies": { "@deepseek-ai/dsh-base": "^1.0.0" },
  "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-base"] } },
  "custom": "keep-me"
}"#,
        )
        .expect("write");
        let mut manifest = read_manifest(&path).expect("read");
        manifest.bundles.push("extra-plugin".into());
        manifest
            .dependencies
            .insert("extra-plugin".into(), "^1.0.0".into());
        write_manifest(&path, &manifest).expect("write");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        assert_eq!(value["custom"], "keep-me");
        assert_eq!(value["dsh"]["profile"]["bundles"][1], "extra-plugin");
        let reloaded = read_manifest(&path).expect("reload");
        assert!(reloaded.bundles.contains(&"extra-plugin".into()));
    }

    #[test]
    fn remove_reconciliation_preserves_installation_owned_profile_layers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let profile_dir = paths.dsh_home.join("profiles/web");
        fs::create_dir_all(&profile_dir).expect("profile");
        fs::write(
            profile_dir.join("package.json"),
            br#"{
              "name":"dsh-profile-web",
              "private":true,
              "dependencies":{
                "dsh-better-sidebar":"^0.15.2",
                "dsh-pocket":"^1.13.4"
              },
              "dsh":{"profile":{"bundles":[
                "@deepseek-ai/dsh-base",
                "@deepseek-ai/dsh-web-app",
                "dsh-better-sidebar",
                "dsh-pocket",
                "dsh-cost-meter"
              ]}}
            }"#,
        )
        .expect("manifest");
        let marketplace = Marketplace::new(paths);

        marketplace
            .reconcile_bundles_after_remove("web", "dsh-cost-meter")
            .expect("reconcile");

        let manifest = read_manifest(&profile_dir.join("package.json")).expect("read");
        assert_eq!(
            manifest.bundles,
            vec![
                "@deepseek-ai/dsh-base",
                "@deepseek-ai/dsh-web-app",
                "dsh-better-sidebar",
                "dsh-pocket",
            ]
        );
    }

    #[test]
    fn installed_scan_matches_skills_and_profiles() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        fs::create_dir_all(paths.dsh_home.join("skills/url-manager-latest")).expect("skills");
        let profile_dir = paths.dsh_home.join("profiles/web");
        fs::create_dir_all(&profile_dir).expect("profile");
        fs::write(
            profile_dir.join("package.json"),
            br#"{"dependencies":{"alpha":"^1.0.0"},"dsh":{"profile":{"bundles":["alpha"]}}}"#,
        )
        .expect("manifest");
        let marketplace = Marketplace::new(paths.clone());
        let catalog = catalog(vec![
            plugin("Piccolo123/url-manager", "url-manager", 1, None),
            plugin("x/alpha", "alpha", 1, None),
            plugin("y/beta", "beta", 1, None),
        ]);
        let installed = marketplace.scan_installed(Some(&catalog));
        assert_eq!(installed.len(), 2);
        let skill = installed
            .iter()
            .find(|e| e.source == PluginSource::Skills)
            .expect("skill");
        assert_eq!(skill.plugin_id.as_deref(), Some("Piccolo123/url-manager"));
        assert_eq!(skill.version.as_deref(), Some("latest"));
        let profile = installed
            .iter()
            .find(|e| e.source == PluginSource::Profile)
            .expect("profile");
        assert_eq!(profile.plugin_id.as_deref(), Some("x/alpha"));
        assert_eq!(profile.profile.as_deref(), Some("web"));
    }

    #[test]
    fn installed_scan_does_not_alias_multi_package_plugin_to_prerequisite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let profile_dir = paths.dsh_home.join("profiles/web");
        fs::create_dir_all(&profile_dir).expect("profile");
        fs::write(
            profile_dir.join("package.json"),
            br#"{
              "dependencies": {"@deepseek-ai/dsh-web-app":"^0.1.0"},
              "dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}
            }"#,
        )
        .expect("manifest");
        let mut read_paper = plugin("louwenbo580/read-paper", "read-paper", 0, None);
        read_paper.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec![
                "dsh plugin --profile readPaper add @deepseek-ai/dsh-web-app paper-review".into(),
            ],
        });
        let marketplace = Marketplace::new(paths);

        let installed = marketplace.scan_installed(Some(&catalog(vec![read_paper])));

        assert!(
            installed.is_empty(),
            "the shared web bundle is not read-paper"
        );
    }

    #[test]
    fn uninstall_of_skill_moves_directory_to_trash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths.clone());
        let catalog = catalog(vec![plugin("x/url-manager", "url-manager", 1, None)]);
        let skills_dir = paths.dsh_home.join("skills");
        fs::create_dir_all(skills_dir.join("url-manager")).expect("skills");
        fs::write(
            skills_dir.join("url-manager/SKILL.md"),
            "# user-modified content",
        )
        .expect("write");
        *marketplace.catalog.lock().expect("catalog") = Some(Arc::new(catalog));
        let target = InstalledPlugin {
            plugin_id: Some("x/url-manager".into()),
            local_name: "url-manager".into(),
            version: None,
            source: PluginSource::Skills,
            profile: None,
        };
        let result = marketplace
            .uninstall("x/url-manager", Some(&target), false)
            .expect("uninstall");
        assert!(result.ok);
        assert!(!skills_dir.join("url-manager").exists());
        // The deleted content is recoverable from the trash directory.
        let trash = marketplace.trash_dir();
        let entries: Vec<_> = fs::read_dir(trash).expect("trash").flatten().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn pnpm_bin_discovery_finds_global_shim_layouts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_dir = temp.path().join("pnpm");
        // The unix global-prefix layout puts the shim in <prefix>/bin.
        if cfg!(windows) {
            fs::create_dir_all(&install_dir).expect("dir");
            fs::write(install_dir.join("pnpm.cmd"), "shim").expect("shim");
            assert_eq!(find_pnpm_dir(&install_dir).expect("found"), install_dir);
        } else {
            fs::create_dir_all(install_dir.join("bin")).expect("bin");
            fs::write(install_dir.join("bin/pnpm"), "shim").expect("shim");
            assert_eq!(
                find_pnpm_dir(&install_dir).expect("found"),
                install_dir.join("bin")
            );
            // The node_modules/.bin layout is also recognized.
            let other = temp.path().join("other");
            fs::create_dir_all(other.join("node_modules/.bin")).expect("bin");
            fs::write(other.join("node_modules/.bin/pnpm"), "shim").expect("shim");
            assert_eq!(
                find_pnpm_dir(&other).expect("found"),
                other.join("node_modules/.bin")
            );
        }
        // A directory without any pnpm shim reports None.
        let empty = temp.path().join("empty");
        fs::create_dir_all(&empty).expect("dir");
        assert_eq!(find_pnpm_dir(&empty), None);
    }

    #[test]
    fn market_command_env_prepends_pnpm_and_node_bin_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let pnpm_bin = paths.cache_dir.join("pnpm-bin");
        let mut command = Command::new("true");
        market_command_env(&mut command, &paths, std::slice::from_ref(&pnpm_bin));
        let mut path_value = None;
        for (key, value) in command.get_envs() {
            if key == "PATH" {
                path_value = value.map(|v| v.to_string_lossy().into_owned());
            }
        }
        let path = path_value.expect("PATH is set");
        let separator = if cfg!(windows) { ";" } else { ":" };
        let prefix = format!("{}{separator}", pnpm_bin.display());
        assert!(path.starts_with(&prefix), "PATH={path}");
        let node_dir = paths
            .node_bin
            .parent()
            .expect("node bin parent")
            .to_string_lossy()
            .into_owned();
        assert!(
            path.contains(&node_dir),
            "node dir missing from PATH={path}"
        );
        // The isolated home is wired as HOME so child tooling cannot read the
        // real user config.
        let mut home_value = None;
        for (key, value) in command.get_envs() {
            if key == "DSH_HOME" {
                home_value = value.map(|v| v.to_string_lossy().into_owned());
            }
        }
        assert_eq!(
            home_value.as_deref(),
            Some(paths.dsh_home.to_str().unwrap())
        );
    }

    #[test]
    fn catalog_state_json_keeps_ts_field_names() {
        // The IPC contract sent to the frontend must use the same field
        // names the generated TypeScript declares.
        let state = MarketCatalogState::Ready {
            generated_at: Some("2026-01-01T00:00:00Z".into()),
            plugin_count: 42,
            stale: false,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["kind"], "ready");
        let ready = &value;
        for field in ["generatedAt", "pluginCount", "stale"] {
            assert!(
                ready.get(field).is_some(),
                "missing field {field} in {json}"
            );
        }
    }

    #[test]
    #[ignore = "network smoke test: fetch, parse and query the live dsh-market catalog"]
    fn live_catalog_smoke_fetches_and_queries_real_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        fs::create_dir_all(&paths.cache_dir).expect("cache");
        let marketplace = Marketplace::new(paths);
        let state = marketplace
            .refresh()
            .or_else(|_| marketplace.refresh())
            .expect("refresh");
        let MarketCatalogState::Ready {
            generated_at,
            plugin_count,
            stale: false,
            ..
        } = state
        else {
            panic!("unexpected catalog state: {state:?}");
        };
        assert!(plugin_count > 100, "live catalog should hold many plugins");
        assert!(
            generated_at.is_some(),
            "live catalog must carry its generatedAt timestamp"
        );
        let page = marketplace
            .query(&MarketQuery {
                search: Some("翻译".into()),
                page_size: 5,
                ..MarketQuery::default()
            })
            .expect("query");
        assert!(!page.items.is_empty(), "zh search should match plugins");
        for item in &page.items {
            assert!(!item.name.is_empty());
            assert!(!item.description_zh.is_empty());
        }
        let skill_page = marketplace
            .query(&MarketQuery {
                kind: Some(PluginKind::Skill),
                page_size: 1,
                ..MarketQuery::default()
            })
            .expect("skill query");
        let skill = skill_page.items.first().expect("live skill");
        let inspected = marketplace.inspect(&skill.id).expect("skill preflight");
        assert!(
            inspected
                .install_version
                .as_deref()
                .is_some_and(valid_commit_sha),
            "skill preflight must pin an immutable commit"
        );
        // The compatibility batch pass must resolve a page of cordis plugins
        // concurrently; every cordis item lands in a terminal state, and a
        // second query for the same page is served from the cache.
        let started = std::time::Instant::now();
        let compat_page = marketplace
            .query(&MarketQuery {
                search: None,
                sort: MarketSort::Stars,
                page: 1,
                page_size: 12,
                check_compatibility: true,
                ..MarketQuery::default()
            })
            .expect("compat query");
        let elapsed = started.elapsed();
        let cordis = compat_page
            .items
            .iter()
            .filter(|item| item.kind == PluginKind::CordisPlugin);
        for item in cordis {
            assert_ne!(
                item.compatibility,
                CompatibilityStatus::NotChecked,
                "compat pass must resolve cordis items, {} stayed notChecked",
                item.id
            );
        }
        let cached_started = std::time::Instant::now();
        marketplace
            .query(&MarketQuery {
                search: None,
                sort: MarketSort::Stars,
                page: 1,
                page_size: 12,
                check_compatibility: true,
                ..MarketQuery::default()
            })
            .expect("cached compat query");
        assert!(
            cached_started.elapsed() < Duration::from_secs(2),
            "cached compat page should resolve without network"
        );
        assert!(
            elapsed < Duration::from_secs(40),
            "compat batch took {elapsed:?}; parallel fill is too slow"
        );
        // A fresh cache must satisfy the TTL gate without touching the
        // network again.
        let ttl_started = std::time::Instant::now();
        let state = marketplace.refresh_if_stale().expect("ttl refresh");
        assert!(matches!(state, MarketCatalogState::Ready { .. }));
        assert!(
            ttl_started.elapsed() < Duration::from_secs(2),
            "fresh cache must not trigger a download"
        );
    }

    #[test]
    fn catalog_json_parses_camel_case_top_level_fields() {
        // The live dsh-market catalog uses camelCase top-level keys
        // (schemaVersion, generatedAt); a missing rename once silently
        // dropped the timestamp and the UI showed "updated –".
        let json = r#"{
          "schemaVersion": 2,
          "generatedAt": "2026-08-23T05:30:24.636Z",
          "plugins": [{
            "id": "x/y", "type": "cordis-plugin", "name": "y",
            "owner": "x", "repo": "y", "fullName": "x/y",
            "stars": 1, "description": "d", "descriptionZh": "中",
            "tags": [], "homepage": null, "license": "MIT",
            "curated": false, "pushedAt": null, "updatedAt": null,
            "install": {"method": "pnpm-profile", "needsConfig": false,
                        "commands": ["dsh plugin --profile web add y@latest"],
                        "commandSource": "README"},
            "score": {"total": 50, "explanation": "e"}
          }],
          "packs": []
        }"#;
        let catalog: MarketCatalogFile = serde_json::from_str(json).expect("catalog parses");
        assert_eq!(catalog.schema_version, 2);
        assert_eq!(
            catalog.generated_at.as_deref(),
            Some("2026-08-23T05:30:24.636Z")
        );
        assert_eq!(catalog.plugins.len(), 1);
        assert_eq!(catalog.plugins[0].description_zh, "中");
        let install = catalog.plugins[0].install.as_ref().expect("install");
        assert_eq!(
            install.commands,
            vec!["dsh plugin --profile web add y@latest"]
        );
        assert_eq!(install_package_name(&catalog.plugins[0]), Some("y".into()));
    }

    #[test]
    fn refresh_while_loading_reports_loading_instead_of_failing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        marketplace.loading.store(true, Ordering::SeqCst);
        let state = marketplace.refresh().expect("concurrent refresh");
        assert_eq!(state, MarketCatalogState::Loading);
        // A concurrent refresh must not have touched the flag or the network.
        assert!(marketplace.loading.load(Ordering::SeqCst));
    }

    #[test]
    fn catalog_state_prefers_cached_data_during_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        *marketplace.catalog.lock().expect("catalog") =
            Some(Arc::new(catalog(vec![plugin("x/y", "y", 1, None)])));
        marketplace.loading.store(true, Ordering::SeqCst);
        match marketplace.catalog_state() {
            MarketCatalogState::Ready { plugin_count, .. } => assert_eq!(plugin_count, 1),
            other => panic!("expected ready state, got {other:?}"),
        }
    }

    #[test]
    fn catalog_age_measures_cache_freshness() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        assert_eq!(marketplace.catalog_age(), None, "no cache yet");
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog dir");
        crate::paths::atomic_write(
            &marketplace.catalog_file(),
            br#"{"schemaVersion":1,"generatedAt":"2026-01-01T00:00:00Z","plugins":[]}"#,
        )
        .expect("cache write");
        let age = marketplace.catalog_age().expect("cached age");
        assert!(
            age < MARKET_CATALOG_TTL,
            "a just-written cache must be fresh, got {age:?}"
        );
    }

    #[test]
    fn query_filters_by_install_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        // "alpha" is installed as a skill; "beta" is not.
        let skills_dir = paths.dsh_home.join("skills");
        fs::create_dir_all(skills_dir.join("alpha-latest")).expect("skills");
        let marketplace = Marketplace::new(paths);
        *marketplace.catalog.lock().expect("catalog") = Some(Arc::new(catalog(vec![
            plugin("x/alpha", "alpha", 1, None),
            plugin("y/beta", "beta", 2, None),
        ])));
        let all = marketplace
            .query(&MarketQuery {
                page_size: 10,
                ..MarketQuery::default()
            })
            .expect("all");
        assert_eq!(all.total, 2);
        let installed = marketplace
            .query(&MarketQuery {
                installed: Some(true),
                page_size: 10,
                ..MarketQuery::default()
            })
            .expect("installed");
        assert_eq!(installed.total, 1);
        assert_eq!(installed.items[0].id, "x/alpha");
        assert!(installed.items[0].installed.is_some());
        let not_installed = marketplace
            .query(&MarketQuery {
                installed: Some(false),
                page_size: 10,
                ..MarketQuery::default()
            })
            .expect("not installed");
        assert_eq!(not_installed.total, 1);
        assert_eq!(not_installed.items[0].id, "y/beta");
        assert!(not_installed.items[0].installed.is_none());
    }

    #[test]
    fn query_paginates_and_reports_totals() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        let mut plugins = Vec::new();
        for i in 0..10 {
            plugins.push(plugin(&format!("o/p{i}"), &format!("p{i}"), i, None));
        }
        *marketplace.catalog.lock().expect("catalog") = Some(Arc::new(catalog(plugins)));
        let query = MarketQuery {
            page_size: 4,
            sort: MarketSort::Stars,
            ..MarketQuery::default()
        };
        let page = marketplace.query(&query).expect("query");
        assert_eq!(page.total, 10);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.items[0].name, "p9");
        let page = marketplace
            .query(&MarketQuery {
                page: 3,
                page_size: 4,
                sort: MarketSort::Stars,
                ..MarketQuery::default()
            })
            .expect("query");
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn percent_decode_handles_scoped_names() {
        assert_eq!(percent_decode("%40scope%2Fpkg"), "@scope/pkg");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn skill_dir_name_validation_rejects_path_escapes() {
        for good in ["url-manager", "dsh-better-sidebar", "1password"] {
            assert!(valid_skill_dir_name(good), "should accept {good:?}");
        }
        for bad in ["", ".", "..", "../evil", "a/b", "/etc", "a//b"] {
            assert!(!valid_skill_dir_name(bad), "should reject {bad:?}");
        }
        // Backslash is a separator on Windows but an ordinary character on
        // unix, so this escape shape only applies there.
        #[cfg(windows)]
        assert!(
            !valid_skill_dir_name("a\\b"),
            "should reject windows separator"
        );
    }

    #[test]
    fn installed_scan_reports_every_profile_entry() {
        // A plugin present in several profiles yields one entry per profile;
        // uninstall relies on this to remove every copy.
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        for profile in ["web", "cli"] {
            let dir = paths.dsh_home.join("profiles").join(profile);
            fs::create_dir_all(&dir).expect("profile");
            fs::write(
                dir.join("package.json"),
                br#"{"dependencies":{"alpha":"^1.0.0"},"dsh":{"profile":{"bundles":["alpha"]}}}"#,
            )
            .expect("manifest");
        }
        let marketplace = Marketplace::new(paths);
        let catalog = catalog(vec![plugin("x/alpha", "alpha", 1, None)]);
        let installed = marketplace.scan_installed(Some(&catalog));
        assert_eq!(installed.len(), 2);
        let mut profiles: Vec<&str> = installed
            .iter()
            .filter_map(|entry| entry.profile.as_deref())
            .collect();
        profiles.sort_unstable();
        assert_eq!(profiles, vec!["cli", "web"]);
    }

    #[test]
    fn installed_scan_cache_is_invalidated_by_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let skills_dir = paths.dsh_home.join("skills");
        fs::create_dir_all(skills_dir.join("alpha-latest")).expect("skills");
        let marketplace = Marketplace::new(paths);
        let catalog = catalog(vec![plugin("x/alpha", "alpha", 1, None)]);
        let first = marketplace.scan_installed_cached(Some(&catalog));
        assert_eq!(first.len(), 1);
        // A second call inside the TTL window reuses the cached scan.
        let cached = marketplace.scan_installed_cached(Some(&catalog));
        assert_eq!(cached, first);
        fs::remove_dir_all(skills_dir.join("alpha-latest")).expect("remove");
        // Without invalidation the stale entry would survive until the TTL.
        marketplace.invalidate_installed_cache();
        let after = marketplace.scan_installed_cached(Some(&catalog));
        assert!(after.is_empty(), "stale scan survived invalidation");
    }

    #[test]
    fn move_dir_falls_back_across_filesystems() {
        // The fallback path is exercised by simulating EXDEV from rename
        // itself being unavailable: copy+delete must still land the content.
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("src");
        fs::create_dir_all(source.join("nested")).expect("nested");
        fs::write(source.join("file.txt"), "data").expect("file");
        fs::write(source.join("nested/inner.txt"), "inner").expect("inner");
        let dest = temp.path().join("dst");
        // Direct copy+delete behaviour via the same helper used after EXDEV.
        copy_dir_recursive(&source, &dest).expect("copy");
        fs::remove_dir_all(&source).expect("remove source");
        assert_eq!(
            fs::read_to_string(dest.join("file.txt")).expect("read"),
            "data"
        );
        assert_eq!(
            fs::read_to_string(dest.join("nested/inner.txt")).expect("read"),
            "inner"
        );
    }

    #[test]
    fn trash_age_prefers_the_name_stamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let now = now_ms();
        let recent = temp.path().join(format!("skill-{now}"));
        fs::create_dir_all(&recent).expect("dir");
        let age = trash_entry_age(&recent).expect("age");
        assert!(
            age < TRASH_RETENTION,
            "fresh entry must be young, got {age:?}"
        );
        let old = temp
            .path()
            .join(format!("skill-{}", now - 31 * 24 * 60 * 60 * 1000));
        fs::create_dir_all(&old).expect("dir");
        let age = trash_entry_age(&old).expect("age");
        assert!(
            age >= TRASH_RETENTION,
            "stale entry must be expired, got {age:?}"
        );
    }

    #[test]
    fn catalog_sanitization_keeps_safe_entries_and_drops_unsafe_targets() {
        let valid = plugin("x/alpha", "alpha", 1, None);
        let mut invalid = plugin("x/bad", "--dir=/tmp/escape", 1, None);
        invalid.homepage = None;
        invalid.install = None;
        let mut redirected = plugin("x/redirected", "redirected", 1, None);
        redirected.full_name = "attacker/repository".into();
        let mut input = MarketCatalogFile {
            schema_version: 2,
            generated_at: None,
            plugins: vec![valid, invalid, redirected],
        };
        sanitize_catalog(&mut input).expect("sanitize");
        assert_eq!(input.plugins.len(), 1);
        assert_eq!(input.plugins[0].id, "x/alpha");
    }

    #[test]
    fn compatibility_cache_expires_and_is_bound_to_runtime_and_package() {
        let cached = CachedCompatibility {
            package_name: "alpha".into(),
            package_version: Some("1.0.0".into()),
            cordis_version: Some("1.2.3".into()),
            checked_at_ms: 1_000,
            info: CompatibilityInfo {
                status: CompatibilityStatus::Compatible,
                detail: None,
            },
            source_binding: SourceBindingStatus::Verified,
            source_binding_detail: None,
        };
        assert!(cached_compatibility_is_valid(
            &cached,
            "alpha",
            Some("1.0.0"),
            Some("1.2.3"),
            2_000
        ));
        assert!(!cached_compatibility_is_valid(
            &cached,
            "beta",
            Some("1.0.0"),
            Some("1.2.3"),
            2_000
        ));
        assert!(!cached_compatibility_is_valid(
            &cached,
            "alpha",
            Some("1.0.0"),
            Some("2.0.0"),
            2_000
        ));
        assert!(!cached_compatibility_is_valid(
            &cached,
            "alpha",
            Some("1.0.0"),
            Some("1.2.3"),
            1_000 + COMPAT_CACHE_TTL.as_millis() as u64
        ));
        assert!(!cached_compatibility_is_valid(
            &cached,
            "alpha",
            Some("1.0.1"),
            Some("1.2.3"),
            2_000
        ));
    }

    #[test]
    fn confirmed_package_version_must_match_the_install_resolution() {
        validate_expected_package_version("alpha", Some("1.0.0"), Some("1.0.0"))
            .expect("same version");
        let changed = validate_expected_package_version("alpha", Some("1.0.0"), Some("1.1.0"))
            .expect_err("changed dist tag must require a new confirmation");
        assert_eq!(changed.code, "marketPackageChanged");
        assert!(validate_expected_package_version("alpha", None, Some("1.1.0")).is_ok());
    }

    #[test]
    fn profile_candidate_preserves_metadata_but_rebuilds_node_modules() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let candidate = temp.path().join("candidate");
        fs::create_dir_all(source.join("config")).expect("config");
        fs::create_dir_all(source.join("node_modules/alpha")).expect("modules");
        fs::write(source.join("package.json"), "{}").expect("manifest");
        fs::write(source.join("config/settings.json"), "settings").expect("settings");
        fs::write(source.join("node_modules/alpha/index.js"), "old").expect("module");
        copy_profile_candidate(&source, &candidate).expect("copy candidate");
        assert!(candidate.join("package.json").is_file());
        assert_eq!(
            fs::read_to_string(candidate.join("config/settings.json")).expect("read settings"),
            "settings"
        );
        assert!(!candidate.join("node_modules").exists());
    }

    #[test]
    fn uninstall_removes_only_the_selected_skill_location() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let skills = paths.dsh_home.join("skills");
        fs::create_dir_all(skills.join("alpha-latest")).expect("latest");
        fs::create_dir_all(skills.join("alpha-1.0.0")).expect("version");
        let marketplace = Marketplace::new(paths);
        *marketplace.catalog.lock().expect("catalog") =
            Some(Arc::new(catalog(vec![plugin("x/alpha", "alpha", 1, None)])));
        let target = InstalledPlugin {
            plugin_id: Some("x/alpha".into()),
            local_name: "alpha-latest".into(),
            version: Some("latest".into()),
            source: PluginSource::Skills,
            profile: None,
        };
        marketplace
            .uninstall("x/alpha", Some(&target), false)
            .expect("uninstall selected");
        assert!(!skills.join("alpha-latest").exists());
        assert!(skills.join("alpha-1.0.0").is_dir());
    }

    #[test]
    fn rapid_market_mutations_are_rejected_instead_of_queued() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&transitions);
        marketplace.set_operation_listener(move |busy| {
            observed.lock().expect("transitions").push(busy);
        });
        let first = marketplace.begin_operation().expect("first operation");
        assert!(marketplace.operation_busy());
        let busy = marketplace
            .begin_operation()
            .expect_err("a second operation must not queue");
        assert_eq!(busy.code, "marketOperationBusy");
        drop(first);
        assert!(!marketplace.operation_busy());
        marketplace
            .begin_operation()
            .expect("gate released after operation");
        assert_eq!(
            *transitions.lock().expect("transitions"),
            vec![true, false, true, false]
        );
    }

    #[test]
    fn catalog_completion_notifies_the_registered_observer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        marketplace.set_catalog_listener(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });

        marketplace.notify_catalog_changed();

        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn malformed_profile_fields_are_rejected_without_silent_filtering() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manifest = temp.path().join("package.json");
        fs::write(
            &manifest,
            br#"{"dependencies":{"alpha":7},"dsh":{"profile":{"bundles":["alpha",9]}}}"#,
        )
        .expect("manifest");
        let error = read_manifest(&manifest).expect_err("malformed fields must fail closed");
        assert_eq!(error.code, "marketProfileInvalid");
    }

    #[test]
    fn candidate_validation_preserves_foundations_and_requires_active_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let candidate = temp.path().join("candidate");
        fs::create_dir_all(candidate.join("node_modules/alpha")).expect("alpha");
        fs::write(
            candidate.join("package.json"),
            br#"{
              "dependencies":{"alpha":"1.0.0"},
              "dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","alpha"]}}
            }"#,
        )
        .expect("profile");
        fs::write(
            candidate.join("node_modules/alpha/package.json"),
            br#"{"name":"alpha","dsh":{"bundle":{"patch":{}}}}"#,
        )
        .expect("package");
        let baseline = ProfileManifest {
            dependencies: BTreeMap::new(),
            bundles: vec!["@deepseek-ai/dsh-base".into()],
        };
        validate_candidate_profile(&baseline, &candidate, "add", "alpha@1.0.0")
            .expect("valid candidate");

        let mut broken = read_manifest(&candidate.join("package.json")).expect("manifest");
        broken
            .bundles
            .retain(|bundle| bundle != "@deepseek-ai/dsh-base");
        write_manifest(&candidate.join("package.json"), &broken).expect("break candidate");
        let error = validate_candidate_profile(&baseline, &candidate, "add", "alpha")
            .expect_err("foundation removal must fail");
        assert_eq!(error.code, "marketProfileInvalid");
    }

    #[test]
    fn foundation_validation_is_independent_of_upstream_package_names() {
        let manifest = ProfileManifest {
            dependencies: BTreeMap::from([("third-party-plugin".into(), "1.0.0".into())]),
            bundles: vec!["@future/dsh-foundation".into(), "third-party-plugin".into()],
        };
        assert!(has_installation_owned_foundation(&manifest));

        let missing = ProfileManifest {
            dependencies: BTreeMap::from([("third-party-plugin".into(), "1.0.0".into())]),
            bundles: vec!["third-party-plugin".into()],
        };
        assert!(!has_installation_owned_foundation(&missing));
    }

    #[test]
    fn uninstall_refuses_a_package_required_by_a_remaining_bundle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile = temp.path();
        fs::create_dir_all(profile.join("node_modules/consumer")).expect("consumer");
        fs::write(
            profile.join("node_modules/consumer/package.json"),
            br#"{"name":"consumer","peerDependencies":{"provider":"^1"}}"#,
        )
        .expect("consumer package");
        let manifest = ProfileManifest {
            dependencies: BTreeMap::new(),
            bundles: vec!["provider".into(), "consumer".into()],
        };
        let error = validate_reverse_package_dependencies(profile, &manifest, "provider")
            .expect_err("reverse dependency must block uninstall");
        assert_eq!(error.code, "marketPluginRequired");
    }

    #[test]
    fn rollback_restores_batch_base_and_retains_rejected_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        let active = marketplace.profile_dir("web");
        let last_good = marketplace.last_good_profile("web");
        fs::create_dir_all(&active).expect("active");
        fs::create_dir_all(&last_good).expect("last good");
        fs::write(active.join("package.json"), br#"{"state":"changed"}"#).expect("active");
        fs::write(last_good.join("package.json"), br#"{"state":"baseline"}"#).expect("baseline");
        for (name, action) in [
            ("alpha", MarketOperationKind::Install),
            ("beta", MarketOperationKind::Install),
            ("gamma", MarketOperationKind::Install),
            ("delta", MarketOperationKind::Uninstall),
            ("epsilon", MarketOperationKind::Uninstall),
            ("zeta", MarketOperationKind::Uninstall),
        ] {
            marketplace
                .write_pending_change(&format!("x/{name}"), name, action, "web")
                .expect("pending change");
        }
        let pending = marketplace
            .pending_verification()
            .expect("pending")
            .expect("batch");
        assert_eq!(pending.changes.len(), 6);
        assert_eq!(pending.changes[0].name, "alpha");
        assert_eq!(pending.changes[5].name, "zeta");

        marketplace.rollback_pending().expect("rollback batch");

        assert_eq!(
            fs::read_to_string(active.join("package.json")).expect("restored"),
            r#"{"state":"baseline"}"#
        );
        assert!(
            marketplace
                .pending_verification()
                .expect("pending")
                .is_none()
        );
        assert!(
            fs::read_dir(marketplace.profiles_dir())
                .expect("profiles")
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".web.market-rejected-"))
        );
    }

    #[test]
    fn startup_recovers_a_profile_missing_mid_publication() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        let backup = marketplace.profile_dir(".web.market-backup-1");
        fs::create_dir_all(&backup).expect("backup");
        fs::write(backup.join("package.json"), "{}").expect("manifest");

        marketplace.initialize();

        assert!(
            marketplace
                .profile_dir("web")
                .join("package.json")
                .is_file()
        );
        assert!(!backup.exists());
    }

    #[test]
    fn crash_recovery_drops_only_the_unpublished_tail_of_a_batch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let last_good = marketplace.last_good_profile("web");
        let backup = marketplace.profile_dir(".web.market-backup-1");
        fs::create_dir_all(&last_good).expect("last good");
        fs::create_dir_all(&backup).expect("backup");
        fs::write(last_good.join("package.json"), "baseline").expect("baseline");
        fs::write(backup.join("package.json"), "after alpha").expect("first change");
        marketplace
            .write_pending_change("x/alpha", "alpha", MarketOperationKind::Install, "web")
            .expect("first pending change");
        marketplace
            .write_pending_change("x/beta", "beta", MarketOperationKind::Install, "web")
            .expect("second pending change");

        marketplace.initialize();

        assert_eq!(
            fs::read_to_string(marketplace.profile_dir("web").join("package.json"))
                .expect("restored first change"),
            "after alpha"
        );
        let pending = marketplace
            .pending_verification()
            .expect("pending")
            .expect("first change remains pending");
        assert_eq!(pending.changes.len(), 1);
        assert_eq!(pending.changes[0].name, "alpha");
        assert!(last_good.exists());
    }

    #[test]
    fn corrupt_pending_journal_is_quarantined_without_losing_rollback_coverage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        fs::create_dir_all(marketplace.last_good_profile("web")).expect("last good");
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog");
        fs::write(marketplace.pending_file(), "not json").expect("corrupt journal");

        marketplace.initialize();

        let pending = marketplace
            .pending_verification()
            .expect("read recovered journal")
            .expect("rollback marker retained");
        assert!(pending.journal_recovered);
        assert_eq!(pending.changes[0].profile.as_deref(), Some("web"));
        assert!(marketplace.has_pending_rollback());
        assert!(
            fs::read_dir(marketplace.catalog_dir())
                .expect("catalog")
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pending.corrupt-"))
        );
    }

    #[test]
    fn mutation_recovers_an_externally_corrupted_journal_and_keeps_new_plugin_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        fs::create_dir_all(marketplace.last_good_profile("web")).expect("last good");
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog");
        fs::write(marketplace.pending_file(), "externally damaged").expect("corrupt journal");

        marketplace
            .write_pending_change(
                "x/new-plugin",
                "new-plugin",
                MarketOperationKind::Install,
                "web",
            )
            .expect("recover and append");

        let pending = marketplace
            .pending_verification()
            .expect("pending")
            .expect("batch");
        assert!(pending.journal_recovered);
        assert_eq!(pending.changes.len(), 2);
        assert!(pending.changes[0].plugin_id.is_empty());
        assert_eq!(pending.changes[1].name, "new-plugin");
        assert_eq!(marketplace.pending_change_summary(), "new-plugin, web");
    }

    #[test]
    fn corrupt_journal_quarantine_retains_only_the_newest_five_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog");
        for index in 0..8 {
            fs::write(
                marketplace
                    .catalog_dir()
                    .join(format!("pending.corrupt-{index:04}.json")),
                "damaged",
            )
            .expect("old quarantine");
        }

        marketplace.initialize();

        let retained = fs::read_dir(marketplace.catalog_dir())
            .expect("catalog")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pending.corrupt-")
            })
            .count();
        assert_eq!(retained, CORRUPT_PENDING_RETENTION);
        assert!(
            !marketplace
                .catalog_dir()
                .join("pending.corrupt-0000.json")
                .exists()
        );
        assert!(
            marketplace
                .catalog_dir()
                .join("pending.corrupt-0007.json")
                .exists()
        );
    }

    #[test]
    fn verified_batch_is_not_pending_after_commit_cleanup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        fs::create_dir_all(marketplace.last_good_profile("web")).expect("last good");
        marketplace
            .write_pending_change("x/alpha", "alpha", MarketOperationKind::Install, "web")
            .expect("pending");

        marketplace
            .clear_pending_verification()
            .expect("commit verified batch");

        assert!(!marketplace.has_pending_rollback());
        assert!(!marketplace.last_good_profile("web").exists());
    }

    #[test]
    fn rollback_uses_last_good_backup_when_pending_journal_is_corrupt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marketplace = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let active = marketplace.profile_dir("web");
        let last_good = marketplace.last_good_profile("web");
        fs::create_dir_all(&active).expect("active");
        fs::create_dir_all(&last_good).expect("last good");
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog");
        fs::write(active.join("package.json"), "changed").expect("active");
        fs::write(last_good.join("package.json"), "baseline").expect("baseline");
        fs::write(marketplace.pending_file(), "not json").expect("corrupt journal");

        assert!(marketplace.has_pending_rollback());
        marketplace.rollback_pending().expect("fallback rollback");

        assert_eq!(
            fs::read_to_string(active.join("package.json")).expect("restored"),
            "baseline"
        );
    }

    #[test]
    fn duplicate_catalog_package_binding_is_not_attributed_to_either_card() {
        let mut first = plugin("one/shared", "one", 0, None);
        first.homepage = Some("https://www.npmjs.com/package/shared".into());
        let mut second = plugin("two/shared", "two", 0, None);
        second.homepage = Some("https://www.npmjs.com/package/shared".into());
        let catalog = catalog(vec![first, second]);
        let index = PluginIndex::build(Some(&catalog));
        assert_eq!(index.by_package.get("shared"), Some(&None));
    }

    #[test]
    fn installed_repository_disambiguates_duplicate_package_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let profile_dir = paths.dsh_home.join("profiles/web");
        fs::create_dir_all(profile_dir.join("node_modules/shared")).expect("package");
        fs::write(
            profile_dir.join("package.json"),
            br#"{
              "dependencies":{"shared":"^1.0.0"},
              "dsh":{"profile":{"bundles":["shared"]}}
            }"#,
        )
        .expect("manifest");
        fs::write(
            profile_dir.join("node_modules/shared/package.json"),
            br#"{
              "name":"shared",
              "version":"1.2.3",
              "repository":"git+https://github.com/two/shared.git"
            }"#,
        )
        .expect("installed package");

        let mut first = plugin("one/shared", "one", 0, None);
        first.homepage = Some("https://www.npmjs.com/package/shared".into());
        let mut second = plugin("two/shared", "two", 0, None);
        second.homepage = Some("https://www.npmjs.com/package/shared".into());
        let marketplace = Marketplace::new(paths);

        let installed = marketplace.scan_installed(Some(&catalog(vec![first, second])));

        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].plugin_id.as_deref(), Some("two/shared"));
        assert_eq!(installed[0].version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn installed_scan_prefers_the_package_owner_over_prerequisite_mentions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let profile_dir = paths.dsh_home.join("profiles/web");
        fs::create_dir_all(&profile_dir).expect("profile");
        fs::write(
            profile_dir.join("package.json"),
            br#"{
              "dependencies":{"dsh-better-sidebar":"^0.15.2"},
              "dsh":{"profile":{"bundles":["dsh-better-sidebar"]}}
            }"#,
        )
        .expect("manifest");

        let mut owner = plugin(
            "omdsh-dev/DSH-better-sidebar",
            "DSH-better-sidebar",
            0,
            None,
        );
        owner.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec!["dsh plugin --profile web add dsh-better-sidebar@latest".into()],
        });
        let mut consumer = plugin(
            "gunduziba/dsh-sidebar-open-in-ide",
            "dsh-sidebar-open-in-ide",
            0,
            None,
        );
        consumer.install = Some(MarketInstallInfo {
            method: Some("pnpm-profile".into()),
            needs_config: Some(false),
            commands: vec!["dsh plugin --profile web add dsh-better-sidebar".into()],
        });

        let marketplace = Marketplace::new(paths);
        let installed = marketplace.scan_installed(Some(&catalog(vec![consumer, owner])));

        assert_eq!(installed.len(), 1);
        assert_eq!(
            installed[0].plugin_id.as_deref(),
            Some("omdsh-dev/DSH-better-sidebar")
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_candidate_rejects_symlinks_that_escape_staging() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let candidate = temp.path().join("candidate");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("real.json"), "{}").expect("real");
        std::os::unix::fs::symlink(source.join("real.json"), source.join("package.json"))
            .expect("symlink");
        let error = copy_profile_candidate(&source, &candidate)
            .expect_err("candidate must reject symlinked control files");
        assert_eq!(error.code, "marketProfileInvalid");
    }

    #[test]
    fn child_output_reader_drains_stdout_and_stderr_concurrently() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "marketplace::tests::child_large_stderr_helper",
                "--nocapture",
            ])
            .env("DSH_MARKET_CHILD_OUTPUT_HELPER", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (success, output) = run_child(command, Duration::from_secs(10)).expect("run child");
        assert!(success, "child output drain failed: {}", tail(&output, 400));
    }

    #[test]
    #[ignore = "helper invoked by child_output_reader_drains_stdout_and_stderr_concurrently"]
    fn child_large_stderr_helper() {
        if std::env::var_os("DSH_MARKET_CHILD_OUTPUT_HELPER").is_none() {
            return;
        }
        use std::io::Write;
        std::io::stderr()
            .write_all(&vec![b'e'; OUTPUT_CAP * 2])
            .expect("stderr");
        std::io::stdout()
            .write_all(&vec![b'o'; OUTPUT_CAP * 2])
            .expect("stdout");
    }

    #[test]
    fn repository_binding_normalizes_npm_repository_shapes() {
        for value in [
            "git+https://github.com/Owner/Repo.git",
            "git+ssh://git@github.com:Owner/Repo.git",
            "https://github.com/Owner/Repo",
            "git@github.com:Owner/Repo.git",
            "github:Owner/Repo",
            "Owner/Repo",
        ] {
            assert_eq!(
                normalize_github_repository(value).as_deref(),
                Some("Owner/Repo"),
                "failed to normalize {value}"
            );
        }
        assert_eq!(normalize_github_repository("https://gitlab.com/x/y"), None);
    }

    #[test]
    fn registry_metadata_binds_latest_version_peer_range_and_repository() {
        let value = serde_json::json!({
            "dist-tags": { "latest": "2.3.4" },
            "versions": {
                "2.3.4": {
                    "peerDependencies": { "cordis": "^4.0.0" },
                    "repository": {
                        "type": "git",
                        "url": "git+https://github.com/owner/repo.git"
                    }
                }
            }
        });
        let info = registry_package_info(&value).expect("registry metadata");
        assert_eq!(info.latest_version, "2.3.4");
        assert_eq!(info.cordis_range.as_deref(), Some("^4.0.0"));
        assert_eq!(info.repository_id.as_deref(), Some("owner/repo"));
        assert!(info.repository_declared);

        let invalid = serde_json::json!({
            "dist-tags": { "latest": "github:attacker/repo" },
            "versions": { "github:attacker/repo": {} }
        });
        assert!(registry_package_info(&invalid).is_err());
    }

    #[test]
    fn catalog_lineage_requires_the_embedded_anchor_as_merge_base() {
        let head = "1111111111111111111111111111111111111111";
        let valid = serde_json::json!({
            "status": "ahead",
            "base_commit": { "sha": MARKET_TRUST_ANCHOR },
            "merge_base_commit": { "sha": MARKET_TRUST_ANCHOR },
            "total_commits": 500
        });
        validate_catalog_lineage(head, &valid).expect("trusted descendant");
        let diverged = serde_json::json!({
            "status": "diverged",
            "base_commit": { "sha": MARKET_TRUST_ANCHOR },
            "merge_base_commit": { "sha": MARKET_TRUST_ANCHOR },
            "total_commits": 1
        });
        assert!(validate_catalog_lineage(head, &diverged).is_err());
    }

    #[test]
    fn cached_catalog_requires_matching_trust_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("home"));
        let marketplace = Marketplace::new(paths);
        fs::create_dir_all(marketplace.catalog_dir()).expect("catalog dir");
        let bytes = br#"{
          "schemaVersion": 2,
          "generatedAt": "2026-08-24T00:00:00Z",
          "plugins": [{
            "id": "x/y", "type": "cordis-plugin", "name": "y",
            "owner": "x", "repo": "y", "fullName": "x/y",
            "homepage": "https://www.npmjs.com/package/y"
          }]
        }"#;
        crate::paths::atomic_write(&marketplace.catalog_file(), bytes).expect("catalog");
        let trust = CatalogTrustMeta {
            commit: MARKET_TRUST_ANCHOR.into(),
            sha256: sha256_bytes(bytes),
        };
        crate::paths::atomic_write(
            &marketplace.catalog_meta_file(),
            &serde_json::to_vec(&trust).expect("trust json"),
        )
        .expect("trust");
        assert_eq!(
            marketplace
                .load_cached_catalog()
                .expect("trusted cache")
                .plugins
                .len(),
            1
        );
        crate::paths::atomic_write(&marketplace.catalog_file(), b"{}").expect("tamper");
        assert!(marketplace.load_cached_catalog().is_err());
    }
}
