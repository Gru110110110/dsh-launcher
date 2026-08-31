use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use base64::Engine;
use fs2::{FileExt, lock_contended_error};
use reqwest::blocking::Client;
use semver::Version;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use uuid::Uuid;

use crate::{
    ActivityCode, AppError, AppResult, ApplicationPaths,
    child_process::{configure_process_group, new_command},
    log_file::{INSTALL_LOG_MAX_BYTES, trim_log_tail},
    model::{NetworkErrorKind, ProxySettings, ProxyTestFailure, ProxyTestReport, ProxyTestSource},
    network,
    paths::atomic_write,
};

pub const NODE_VERSION: &str = "24.19.0";
const NODE_BASES: [&str; 2] = [
    "https://nodejs.org/dist",
    "https://npmmirror.com/mirrors/node",
];
const NPM_REGISTRIES: [&str; 2] = [
    "https://registry.npmjs.org",
    "https://registry.npmmirror.com",
];
const MAX_NODE_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NODE_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_HARNESS_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const HARNESS_ARCHIVE_DOWNLOAD_ATTEMPTS: usize = 2;
const NPM_CACHE_PRUNE_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;
const RUNTIME_COPY_RESERVE_BYTES: u64 = 512 * 1024 * 1024;
const NPM_PROCESS_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const NPM_FALLBACK_PROCESS_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const NPM_WAITING_AFTER: Duration = Duration::from_secs(30);
const RELEASE_SOURCE_ATTEMPTS: usize = 3;
const RELEASE_NODE_ASSETS: [(&str, &str); 3] = [
    (
        "node-v24.19.0-darwin-arm64.tar.gz",
        "8294b7aa9b03997481c06babf1e8b270c859358f27da57a11509afe537ac381d",
    ),
    (
        "node-v24.19.0-darwin-x64.tar.gz",
        "d1b5e999db158c62fe8f7267a4476b035d8bd93b1a605bac24a3f0dd166e3316",
    ),
    (
        "node-v24.19.0-win-x64.zip",
        "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentEvent {
    Activity {
        code: ActivityCode,
        values: BTreeMap<String, String>,
    },
    Progress {
        done: u64,
        total: Option<u64>,
    },
    ActivityUpdate {
        values: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct DeploymentController {
    cancelled: Arc<AtomicBool>,
    cleanup_error: Arc<Mutex<Option<AppError>>>,
}

impl DeploymentController {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn cleanup_error(&self) -> Option<AppError> {
        self.cleanup_error
            .lock()
            .expect("deployment cleanup error poisoned")
            .clone()
    }
    fn record_cleanup_error(&self, error: AppError) {
        *self
            .cleanup_error
            .lock()
            .expect("deployment cleanup error poisoned") = Some(error);
    }
    fn check(&self) -> AppResult<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(AppError::new("deploymentCancelled"))
        } else {
            Ok(())
        }
    }
}

struct PartialDownload(PathBuf);

impl Drop for PartialDownload {
    fn drop(&mut self) {
        let _ = remove_owned(&self.0);
    }
}

pub fn installed_version(paths: &ApplicationPaths) -> Option<String> {
    let marker = fs::read_to_string(&paths.version_file).ok()?;
    let manifest = dsh_manifest_version(&paths.dsh_dir)?;
    (marker.trim() == manifest).then_some(manifest)
}

pub fn is_runtime_ready(paths: &ApplicationPaths) -> bool {
    let Some(version) = installed_version(paths) else {
        return false;
    };
    let Ok(expected_node) = resolve_node_version() else {
        return false;
    };
    node_version(paths, &paths.node_dir).as_deref() == Some(expected_node.as_str())
        && dsh_valid(paths, &paths.node_dir, &paths.dsh_dir, &version)
}

pub fn latest_harness_version(controller: &DeploymentController) -> AppResult<String> {
    controller.check()?;
    let client = http_client()?;
    let registries = npm_registries();
    // Probe every registry instead of trusting the first one: a blocked or
    // stale authority must not hide a newer release available on a mirror,
    // and the newest observed latest version wins.
    let mut latest = None;
    let mut failures = Vec::new();
    let mut failure_kinds = Vec::new();
    for registry in registries {
        controller.check()?;
        match query_registry_version(&client, &registry) {
            Ok(version) => {
                latest = Some(match latest {
                    Some(current) if current >= version => current,
                    _ => version,
                });
            }
            Err(error) => {
                // Query errors already carry their (sanitized) source prefix;
                // prefixing again would duplicate the registry address.
                failures.push(
                    error.safe_detail.clone().unwrap_or_else(|| {
                        format!("{}: {}", display_source(&registry), error.code)
                    }),
                );
                if let Some(kind) = error.values.get("kind") {
                    failure_kinds.push(NetworkErrorKind::parse(kind));
                }
            }
        }
    }
    latest.map(|version| version.to_string()).ok_or_else(|| {
        let detail = if failures.is_empty() {
            "no npm registry configured".to_owned()
        } else {
            failures.join("; ")
        };
        let kind = primary_version_query_failure(&failure_kinds);
        let code = match kind {
            Some(NetworkErrorKind::ProxyAuth) => "versionQueryProxyAuth",
            Some(NetworkErrorKind::Tls) => "versionQueryTls",
            Some(NetworkErrorKind::Timeout) => "versionQueryTimeout",
            Some(NetworkErrorKind::Connect) => "versionQueryConnect",
            Some(NetworkErrorKind::HttpStatus) => "versionQueryHttpStatus",
            Some(NetworkErrorKind::Other) | None => "versionQueryFailed",
        };
        let error = AppError::new(code).detail(detail);
        match kind {
            Some(kind) => error.value("kind", kind.as_str()),
            None => error,
        }
    })
}

/// Chooses the most actionable transport cause when every registry failed.
/// Proxy authentication and TLS configuration need different user action from
/// an ordinary connection failure, so they take priority when sources fail in
/// different ways. Metadata-only failures carry no kind and retain the generic
/// version-query error.
fn primary_version_query_failure(kinds: &[NetworkErrorKind]) -> Option<NetworkErrorKind> {
    [
        NetworkErrorKind::ProxyAuth,
        NetworkErrorKind::Tls,
        NetworkErrorKind::Timeout,
        NetworkErrorKind::Connect,
        NetworkErrorKind::HttpStatus,
        NetworkErrorKind::Other,
    ]
    .into_iter()
    .find(|candidate| kinds.contains(candidate))
}

pub fn verify_release_sources() -> AppResult<Vec<String>> {
    let client = http_client()?;
    let mut verified = Vec::new();
    for base in NODE_BASES {
        let manifest_url = format!("{base}/v{NODE_VERSION}/SHASUMS256.txt");
        let manifest = retry_release_source(|| {
            client
                .get(&manifest_url)
                .send()
                .and_then(|response| response.error_for_status())
                .and_then(|response| response.text())
                .map_err(|error| {
                    AppError::new("releaseSourceFailed").detail(format!(
                        "{}: {}",
                        display_source(&manifest_url),
                        network::sanitize_detail(&error.to_string())
                    ))
                })
        })?;
        for (filename, expected) in RELEASE_NODE_ASSETS {
            let actual = manifest
                .lines()
                .find_map(|line| {
                    let mut fields = line.split_whitespace();
                    let checksum = fields.next()?;
                    let listed = fields.next()?;
                    (listed.trim_start_matches('*') == filename)
                        .then(|| checksum.to_ascii_lowercase())
                })
                .ok_or_else(|| {
                    AppError::new("releaseSourceFailed")
                        .detail(format!("{} does not list {filename}", display_source(base)))
                })?;
            if actual != expected {
                return Err(AppError::new("releaseSourceFailed").detail(format!(
                    "{} checksum mismatch for {filename}",
                    display_source(base)
                )));
            }
        }
        verified.push(format!(
            "Node {NODE_VERSION} release targets via {}",
            display_source(base)
        ));
    }
    let authority = NPM_REGISTRIES[0];
    let version = retry_release_source(|| query_registry_version(&client, authority))?;
    verified.push(format!(
        "Harness latest {version} via {}",
        display_source(authority)
    ));
    for registry in &NPM_REGISTRIES[1..] {
        retry_release_source(|| query_registry_exact_version(&client, registry, &version))?;
        verified.push(format!(
            "Harness {version} mirror via {}",
            display_source(registry)
        ));
    }
    Ok(verified)
}

fn retry_release_source<T>(mut operation: impl FnMut() -> AppResult<T>) -> AppResult<T> {
    for attempt in 1..=RELEASE_SOURCE_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if attempt == RELEASE_SOURCE_ATTEMPTS => return Err(error),
            Err(_) => thread::sleep(Duration::from_secs(attempt as u64)),
        }
    }
    unreachable!("release source attempts is non-zero")
}

fn query_registry_version(client: &Client, registry: &str) -> AppResult<Version> {
    let value = query_registry_packument(client, registry)?;
    let version = value
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(Value::as_str)
        .and_then(|raw| Version::parse(raw).ok())
        .ok_or_else(|| {
            AppError::new("versionQueryFailed").detail(format!(
                "{}: invalid latest version metadata",
                display_source(registry)
            ))
        })?;
    release_from_packument(registry, &value, &version)?;
    Ok(version)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HarnessInstallSource {
    registry: String,
    tarball: String,
    integrity: String,
}

fn query_registry_exact_version(
    client: &Client,
    registry: &str,
    expected: &Version,
) -> AppResult<HarnessInstallSource> {
    let value = query_registry_packument(client, registry)?;
    release_from_packument(registry, &value, expected)
}

fn query_registry_packument(client: &Client, registry: &str) -> AppResult<Value> {
    validate_network_source(registry)?;
    let url = format!("{registry}/@deepseek-ai%2Fdsh");
    client
        .get(&url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
        .header(reqwest::header::PRAGMA, "no-cache")
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json::<Value>())
        .map_err(|error| {
            let classified = network::classify_reqwest(&error);
            AppError::new("versionQueryFailed")
                .value("kind", classified.kind.as_str())
                .detail(format!(
                    "{}: {}",
                    display_source(registry),
                    classified.detail
                ))
        })
}

fn release_from_packument(
    registry: &str,
    value: &Value,
    expected: &Version,
) -> AppResult<HarnessInstallSource> {
    let release = value
        .get("versions")
        .and_then(|versions| versions.get(expected.to_string()))
        .ok_or_else(|| {
            AppError::new("versionQueryFailed").detail(format!(
                "{}: Harness {expected} is absent from the install version index",
                display_source(registry)
            ))
        })?;
    let actual = release
        .get("version")
        .and_then(Value::as_str)
        .and_then(|raw| Version::parse(raw).ok());
    if actual.as_ref() != Some(expected) {
        return Err(AppError::new("versionQueryFailed").detail(format!(
            "{}: Harness {expected} returned invalid version metadata",
            display_source(registry)
        )));
    }
    let distribution = release.get("dist").ok_or_else(|| {
        AppError::new("versionQueryFailed").detail(format!(
            "{}: Harness {expected} has no downloadable artifact metadata",
            display_source(registry)
        ))
    })?;
    let tarball = distribution
        .get("tarball")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::new("versionQueryFailed").detail(format!(
                "{}: Harness {expected} has no downloadable artifact",
                display_source(registry)
            ))
        })?;
    validate_network_source(tarball)?;
    let integrity = distribution
        .get("integrity")
        .and_then(Value::as_str)
        .and_then(sha512_integrity)
        .ok_or_else(|| {
            AppError::new("versionQueryFailed").detail(format!(
                "{}: Harness {expected} has no supported artifact integrity metadata",
                display_source(registry)
            ))
        })?;
    Ok(HarnessInstallSource {
        registry: registry.to_owned(),
        tarball: tarball.to_owned(),
        integrity: integrity.to_owned(),
    })
}

fn sha512_integrity(value: &str) -> Option<&str> {
    value
        .split_ascii_whitespace()
        .find_map(|entry| entry.strip_prefix("sha512-"))
        .filter(|digest| {
            !digest.is_empty()
                && base64::engine::general_purpose::STANDARD
                    .decode(digest)
                    .is_ok_and(|decoded| decoded.len() == 64)
        })
}

fn ranked_install_registries(
    client: &Client,
    registries: Vec<String>,
    expected: &Version,
    preferred: Option<&str>,
) -> AppResult<Vec<HarnessInstallSource>> {
    let mut available = Vec::new();
    for (index, registry) in registries.into_iter().enumerate() {
        if let Err(error) = validate_network_source(&registry) {
            log::warn!(
                "skipping invalid Harness {expected} source {}: {error}",
                display_source(&registry)
            );
            continue;
        }
        let started = Instant::now();
        match query_registry_exact_version(client, &registry, expected) {
            Ok(source) => available.push((started.elapsed(), index, source)),
            Err(error) => log::warn!(
                "skipping Harness {expected} source {}: {error}",
                display_source(&registry)
            ),
        }
    }
    available.sort_by_key(|(latency, index, source)| {
        (
            preferred != Some(source.registry.as_str()),
            *latency,
            *index,
        )
    });
    Ok(available.into_iter().map(|(_, _, source)| source).collect())
}

fn download_harness_tarball(
    client: &Client,
    source: &HarnessInstallSource,
    cache_dir: &Path,
    deadline: Instant,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<tempfile::TempPath> {
    controller.check()?;
    if Instant::now() >= deadline {
        return Err(AppError::new("downloadTimedOut"));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let mut response = client
        .get(&source.tarball)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .timeout(remaining)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| {
            if error.is_timeout() || Instant::now() >= deadline {
                AppError::new("downloadTimedOut")
            } else {
                AppError::new("downloadFailed").detail(format!(
                    "{}: {}",
                    display_source(&source.tarball),
                    network::sanitize_detail(&error.to_string())
                ))
            }
        })?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HARNESS_ARCHIVE_BYTES)
    {
        return Err(AppError::new("harnessArchiveTooLarge"));
    }

    let mut archive = tempfile::Builder::new()
        .prefix("harness.staging-")
        .suffix(".tgz")
        .tempfile_in(cache_dir)?;
    let total = response.content_length();
    let mut digest = Sha512::new();
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        controller.check()?;
        if Instant::now() >= deadline {
            return Err(AppError::new("downloadTimedOut"));
        }
        let read = response.read(&mut buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut || Instant::now() >= deadline {
                AppError::new("downloadTimedOut")
            } else {
                AppError::io("downloadFailed", &error)
            }
        })?;
        if read == 0 {
            break;
        }
        downloaded = downloaded
            .checked_add(read as u64)
            .ok_or_else(|| AppError::new("harnessArchiveTooLarge"))?;
        if downloaded > MAX_HARNESS_ARCHIVE_BYTES {
            return Err(AppError::new("harnessArchiveTooLarge"));
        }
        archive.write_all(&buffer[..read])?;
        digest.update(&buffer[..read]);
        notify(DeploymentEvent::Progress {
            done: downloaded,
            total,
        });
    }
    archive.flush()?;
    let actual = base64::engine::general_purpose::STANDARD.encode(digest.finalize());
    if actual != source.integrity {
        return Err(AppError::new("checksumFailed").detail(format!(
            "Harness archive from {} did not match its published integrity",
            display_source(&source.registry)
        )));
    }
    Ok(archive.into_temp_path())
}

fn download_harness_tarball_with_retries(
    client: &Client,
    source: &HarnessInstallSource,
    cache_dir: &Path,
    deadline: Instant,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<tempfile::TempPath> {
    let mut errors = Vec::new();
    for attempt in 1..=HARNESS_ARCHIVE_DOWNLOAD_ATTEMPTS {
        match download_harness_tarball(client, source, cache_dir, deadline, controller, notify) {
            Ok(archive) => return Ok(archive),
            Err(error) if error.code == "deploymentCancelled" => return Err(error),
            Err(error) => {
                errors.push(format!(
                    "attempt {attempt}: {}",
                    error
                        .safe_detail
                        .clone()
                        .unwrap_or_else(|| error.code.clone())
                ));
                if Instant::now() >= deadline || error.code == "harnessArchiveTooLarge" {
                    break;
                }
            }
        }
    }
    let code = if Instant::now() >= deadline {
        "downloadTimedOut"
    } else {
        "downloadFailed"
    };
    Err(AppError::new(code).detail(errors.join("; ")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NpmInstallPhase {
    Preparing,
    Resolving,
    Downloading,
    Writing,
}

struct NpmInstallActivity {
    version: String,
    source: String,
    pending: String,
    phase: NpmInstallPhase,
    resolved: u64,
    packages: u64,
    written: u64,
    last_emitted: Option<(NpmInstallPhase, u64, bool)>,
}

impl NpmInstallActivity {
    fn new(version: &str, registry: &str) -> Self {
        Self {
            version: version.to_owned(),
            source: display_source(registry),
            pending: String::new(),
            phase: NpmInstallPhase::Preparing,
            resolved: 0,
            packages: 0,
            written: 0,
            last_emitted: None,
        }
    }

    fn observe(&mut self, output: &str, idle: Duration, notify: &impl Fn(DeploymentEvent)) {
        self.pending.push_str(output);
        if let Some(end) = self.pending.rfind('\n') {
            let tail = self.pending.split_off(end + 1);
            let complete = std::mem::replace(&mut self.pending, tail);
            for line in complete.lines() {
                if line.contains("silly fetch manifest ") {
                    self.resolved = self.resolved.saturating_add(1);
                    self.phase = self.phase.max(NpmInstallPhase::Resolving);
                }
                if line.contains("silly placeDep ") {
                    self.resolved = self.resolved.saturating_add(1);
                    self.phase = self.phase.max(NpmInstallPhase::Resolving);
                }
                if (line.contains("http fetch ") || line.contains("http cache "))
                    && line.contains(".tgz")
                {
                    self.packages = self.packages.saturating_add(1);
                    self.phase = self.phase.max(NpmInstallPhase::Downloading);
                }
                if line.contains("silly ADD ") {
                    self.written = self.written.saturating_add(1);
                    self.phase = NpmInstallPhase::Writing;
                }
            }
        }

        let waiting = idle >= NPM_WAITING_AFTER;
        let processed = match self.phase {
            NpmInstallPhase::Preparing => 0,
            NpmInstallPhase::Resolving => self.resolved,
            NpmInstallPhase::Downloading => self.packages,
            NpmInstallPhase::Writing => self.written,
        };
        let current = (self.phase, processed, waiting);
        if self.last_emitted == Some(current) {
            return;
        }

        let code = match self.phase {
            NpmInstallPhase::Preparing => ActivityCode::InstallingHarness,
            NpmInstallPhase::Resolving => ActivityCode::ResolvingHarnessDependencies,
            NpmInstallPhase::Downloading => ActivityCode::DownloadingHarnessPackages,
            NpmInstallPhase::Writing => ActivityCode::WritingHarnessRuntime,
        };
        let values = BTreeMap::from([
            ("version".to_owned(), self.version.clone()),
            ("source".to_owned(), self.source.clone()),
            ("processed".to_owned(), processed.to_string()),
            (
                "status".to_owned(),
                if waiting { "waiting" } else { "active" }.to_owned(),
            ),
        ]);
        if self
            .last_emitted
            .is_none_or(|(previous, _, _)| previous != self.phase)
        {
            notify(DeploymentEvent::Activity { code, values });
        } else {
            notify(DeploymentEvent::ActivityUpdate { values });
        }
        self.last_emitted = Some(current);
    }
}

pub fn deploy_runtime(
    paths: &ApplicationPaths,
    force: bool,
    target_version: Option<&str>,
    controller: &DeploymentController,
    notify: impl Fn(DeploymentEvent),
) -> AppResult<String> {
    paths.ensure_dirs()?;
    activity(&notify, ActivityCode::WaitingForLock, []);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.deployment_lock)?;
    acquire_lock(&lock_file, controller)?;
    let result = (|| {
        recover_interrupted(paths)?;
        recover_valid_previous(paths)?;
        prune_stale_harness_archives(&paths.cache_dir)?;
        trim_log_tail(&paths.install_log, INSTALL_LOG_MAX_BYTES)?;
        prune_oversized_npm_cache(paths);
        activity(&notify, ActivityCode::CheckingRuntime, []);
        if !force && is_runtime_ready(paths) {
            return installed_version(paths)
                .ok_or_else(|| AppError::new("runtimeValidationFailed"));
        }
        let previous_version = installed_version(paths);
        let version = match target_version {
            Some(value) => Version::parse(value)
                .map_err(|_| AppError::new("runtimeVersionInvalid").value("version", value))?
                .to_string(),
            None => {
                activity(&notify, ActivityCode::ResolvingVersion, []);
                latest_harness_version(controller)?
            }
        };
        let node_previous = ensure_node(paths, controller, &notify)?;
        notify(DeploymentEvent::Progress {
            done: 0,
            total: None,
        });
        let staging = match install_harness(paths, &version, controller, &notify) {
            Ok(staging) => staging,
            Err(error) => {
                if let Some(previous) = node_previous.as_deref() {
                    rollback_directory(&paths.node_dir, Some(previous))?;
                }
                return Err(error);
            }
        };
        activity(
            &notify,
            ActivityCode::ActivatingHarness,
            [("version", version.clone())],
        );
        let dsh_previous = match publish_directory(&staging, &paths.dsh_dir) {
            Ok(previous) => previous,
            Err(error) => {
                if let Some(previous) = node_previous.as_deref() {
                    rollback_directory(&paths.node_dir, Some(previous))?;
                }
                return Err(error);
            }
        };
        if !dsh_valid(paths, &paths.node_dir, &paths.dsh_dir, &version) {
            rollback_directory(&paths.dsh_dir, dsh_previous.as_deref())?;
            if let Some(previous) = node_previous.as_deref() {
                rollback_directory(&paths.node_dir, Some(previous))?;
            }
            return Err(
                AppError::new("runtimeValidationFailed").value("component", "DeepSeek Harness")
            );
        }
        if let Err(error) = atomic_write(&paths.version_file, format!("{version}\n").as_bytes()) {
            rollback_directory(&paths.dsh_dir, dsh_previous.as_deref())?;
            if let Some(previous) = node_previous.as_deref() {
                rollback_directory(&paths.node_dir, Some(previous))?;
            }
            if let Some(old) = previous_version {
                let _ = atomic_write(&paths.version_file, format!("{old}\n").as_bytes());
            }
            return Err(error);
        }
        Ok(version)
    })();
    let _ = FileExt::unlock(&lock_file);
    result
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct PreparedHarnessUpdate {
    version: String,
}

/// Returns a fully installed and validated Harness candidate, if one is ready
/// to be activated. The candidate never replaces the runtime used by the
/// current service until `activate_prepared_harness_update` is called.
pub fn prepared_harness_update(paths: &ApplicationPaths) -> AppResult<Option<String>> {
    let marker_metadata = match fs::symlink_metadata(&paths.pending_harness_update_file) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let candidate_metadata = match fs::symlink_metadata(&paths.pending_dsh_dir) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if marker_metadata.is_none() && candidate_metadata.is_none() {
        return Ok(None);
    }
    let (Some(marker_metadata), Some(candidate_metadata)) = (marker_metadata, candidate_metadata)
    else {
        return Err(AppError::new("preparedHarnessUpdateInvalid"));
    };
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return Err(AppError::new("preparedHarnessUpdateInvalid"));
    }
    if !candidate_metadata.is_dir() || candidate_metadata.file_type().is_symlink() {
        return Err(AppError::new("preparedHarnessUpdateInvalid"));
    }
    let prepared: PreparedHarnessUpdate = serde_json::from_slice(&fs::read(
        &paths.pending_harness_update_file,
    )?)
    .map_err(|error| AppError::new("preparedHarnessUpdateInvalid").detail(error.to_string()))?;
    Version::parse(&prepared.version).map_err(|_| {
        AppError::new("preparedHarnessUpdateInvalid").value("version", &prepared.version)
    })?;
    if !dsh_valid(
        paths,
        &paths.node_dir,
        &paths.pending_dsh_dir,
        &prepared.version,
    ) {
        return Err(
            AppError::new("preparedHarnessUpdateInvalid").value("version", &prepared.version)
        );
    }
    Ok(Some(prepared.version))
}

/// Loads a valid prepared candidate and removes only application-owned,
/// provably invalid candidate artifacts while holding the deployment lock.
/// Transient I/O errors preserve the artifacts for a later recovery attempt.
pub fn recover_prepared_harness_update(paths: &ApplicationPaths) -> AppResult<Option<String>> {
    paths.ensure_dirs()?;
    let controller = DeploymentController::default();
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.deployment_lock)?;
    acquire_lock(&lock_file, &controller)?;
    let result = match prepared_harness_update(paths) {
        Ok(Some(prepared)) => {
            let active_is_same_or_newer = installed_version(paths)
                .and_then(|active| Version::parse(&active).ok())
                .zip(Version::parse(&prepared).ok())
                .is_some_and(|(active, prepared)| active >= prepared);
            if active_is_same_or_newer {
                remove_owned(&paths.pending_harness_update_file)?;
                remove_owned(&paths.pending_dsh_dir)?;
                Ok(None)
            } else {
                Ok(Some(prepared))
            }
        }
        Ok(None) => Ok(None),
        Err(error) if error.code == "preparedHarnessUpdateInvalid" => {
            remove_owned(&paths.pending_harness_update_file)?;
            remove_owned(&paths.pending_dsh_dir)?;
            Ok(None)
        }
        Err(error) => Err(error),
    };
    let _ = FileExt::unlock(&lock_file);
    result
}

/// Removes a prepared runtime candidate after another update path has already
/// committed a newer active runtime.
pub fn discard_prepared_harness_update(paths: &ApplicationPaths) -> AppResult<()> {
    paths.ensure_dirs()?;
    let controller = DeploymentController::default();
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.deployment_lock)?;
    acquire_lock(&lock_file, &controller)?;
    let result = (|| {
        remove_owned(&paths.pending_harness_update_file)?;
        remove_owned(&paths.pending_dsh_dir)
    })();
    let _ = FileExt::unlock(&lock_file);
    result
}

/// Downloads, installs, and validates a Harness update without touching the
/// active runtime. Only the application-owned pending candidate and marker are
/// persisted after validation succeeds.
pub fn prepare_harness_update(
    paths: &ApplicationPaths,
    version: &str,
    controller: &DeploymentController,
    notify: impl Fn(DeploymentEvent),
) -> AppResult<String> {
    let version = Version::parse(version)
        .map_err(|_| AppError::new("runtimeVersionInvalid").value("version", version))?
        .to_string();
    paths.ensure_dirs()?;
    activity(&notify, ActivityCode::WaitingForLock, []);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.deployment_lock)?;
    acquire_lock(&lock_file, controller)?;
    let result = (|| {
        recover_interrupted(paths)?;
        if prepared_harness_update(paths).ok().flatten().as_deref() == Some(version.as_str()) {
            return Ok(version.clone());
        }
        // Invalid or obsolete candidates are application runtime artifacts,
        // never user data. Remove them only while holding the deployment lock.
        remove_owned(&paths.pending_harness_update_file)?;
        remove_owned(&paths.pending_dsh_dir)?;

        let installed = installed_version(paths)
            .ok_or_else(|| AppError::new("backgroundHarnessUpdateUnavailable"))?;
        let expected_node = resolve_node_version()?;
        if node_version(paths, &paths.node_dir).as_deref() != Some(expected_node.as_str())
            || !runtime_pair_valid(paths, &paths.node_dir, &paths.dsh_dir, &installed)
        {
            return Err(AppError::new("backgroundHarnessUpdateUnavailable"));
        }

        prune_stale_harness_archives(&paths.cache_dir)?;
        trim_log_tail(&paths.install_log, INSTALL_LOG_MAX_BYTES)?;
        let staging = install_harness(paths, &version, controller, &notify)?;
        controller.check()?;
        if !dsh_valid(paths, &paths.node_dir, &staging, &version) {
            remove_owned(&staging)?;
            return Err(
                AppError::new("runtimeValidationFailed").value("component", "DeepSeek Harness")
            );
        }
        fs::rename(&staging, &paths.pending_dsh_dir)?;
        let marker = serde_json::to_vec(&PreparedHarnessUpdate {
            version: version.clone(),
        })
        .map_err(|error| AppError::new("writeFailed").detail(error.to_string()))?;
        if let Err(error) = atomic_write(&paths.pending_harness_update_file, &marker) {
            let _ = remove_owned(&paths.pending_dsh_dir);
            return Err(error);
        }
        Ok(version.clone())
    })();
    let _ = FileExt::unlock(&lock_file);
    result
}

/// Atomically switches a previously validated candidate into service. The
/// previous runtime remains available for rollback until the new version and
/// version index have both been verified.
pub fn activate_prepared_harness_update(
    paths: &ApplicationPaths,
    controller: &DeploymentController,
    notify: impl Fn(DeploymentEvent),
) -> AppResult<String> {
    paths.ensure_dirs()?;
    activity(&notify, ActivityCode::WaitingForLock, []);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.deployment_lock)?;
    acquire_lock(&lock_file, controller)?;
    let result = (|| {
        recover_interrupted(paths)?;
        let version = prepared_harness_update(paths)?
            .ok_or_else(|| AppError::new("preparedHarnessUpdateUnavailable"))?;
        controller.check()?;
        activity(
            &notify,
            ActivityCode::ActivatingHarness,
            [("version", version.clone())],
        );
        let previous_version = installed_version(paths);
        let previous = publish_directory(&paths.pending_dsh_dir, &paths.dsh_dir)?;
        if !dsh_valid(paths, &paths.node_dir, &paths.dsh_dir, &version) {
            rollback_directory(&paths.dsh_dir, previous.as_deref())?;
            let _ = remove_owned(&paths.pending_harness_update_file);
            return Err(
                AppError::new("runtimeValidationFailed").value("component", "DeepSeek Harness")
            );
        }
        if let Err(error) = atomic_write(&paths.version_file, format!("{version}\n").as_bytes()) {
            rollback_directory(&paths.dsh_dir, previous.as_deref())?;
            let _ = remove_owned(&paths.pending_harness_update_file);
            if let Some(previous_version) = previous_version {
                let _ = atomic_write(
                    &paths.version_file,
                    format!("{previous_version}\n").as_bytes(),
                );
            }
            return Err(error);
        }
        if let Err(error) = remove_owned(&paths.pending_harness_update_file) {
            // Activation is already committed and validated. A stale marker is
            // harmless and will be ignored because its candidate is absent.
            log::warn!("prepared Harness update marker could not be removed: {error}");
        }
        Ok(version)
    })();
    let _ = FileExt::unlock(&lock_file);
    result
}

fn ensure_node(
    paths: &ApplicationPaths,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<Option<PathBuf>> {
    let version = resolve_node_version()?;
    let filename = node_filename(&version)?;
    if let Err(error) = prune_old_node_archives(&paths.cache_dir, &filename) {
        log::warn!("old Node.js archives could not be pruned: {error}");
    }
    if node_version(paths, &paths.node_dir).as_deref() == Some(version.as_str()) {
        return Ok(None);
    }
    let checksum = node_checksum(&filename)?;
    let archive = paths.cache_dir.join(&filename);
    activity(
        notify,
        ActivityCode::DownloadingNode,
        [("version", version.clone())],
    );
    download_verified(
        &node_bases()
            .iter()
            .map(|base| format!("{base}/v{version}/{filename}"))
            .collect::<Vec<_>>(),
        &archive,
        &checksum,
        controller,
        notify,
    )?;
    activity(
        notify,
        ActivityCode::VerifyingNode,
        [("version", version.clone())],
    );
    let staging = paths
        .runtime_dir
        .join(format!("node.staging-{}", Uuid::new_v4()));
    extract_node(&archive, &staging)?;
    if node_version(paths, &staging).as_deref() != Some(version.as_str()) {
        remove_owned(&staging)?;
        return Err(AppError::new("runtimeValidationFailed").value("component", "Node.js"));
    }
    let previous = publish_directory(&staging, &paths.node_dir)?;
    if node_version(paths, &paths.node_dir).as_deref() != Some(version.as_str()) {
        rollback_directory(&paths.node_dir, previous.as_deref())?;
        return Err(AppError::new("runtimeValidationFailed").value("component", "Node.js"));
    }
    Ok(previous)
}

fn install_harness(
    paths: &ApplicationPaths,
    version: &str,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<PathBuf> {
    let staging = paths
        .runtime_dir
        .join(format!("dsh.staging-{}", Uuid::new_v4()));
    let result = install_harness_into(paths, &staging, version, controller, notify);
    if let Err(install_error) = &result
        && let Err(cleanup_error) = cleanup_failed_staging(&staging, controller)
    {
        log::error!(
            "Harness install failed: {install_error}; the failed staging directory could not be removed either: {cleanup_error}"
        );
        return Err(cleanup_error);
    }
    if controller.cleanup_error().is_none() {
        prune_oversized_npm_cache(paths);
    }
    result
}

fn install_harness_into(
    paths: &ApplicationPaths,
    staging: &Path,
    version: &str,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<PathBuf> {
    activity(
        notify,
        ActivityCode::CheckingSources,
        [("version", version.to_owned())],
    );
    let client = http_client()?;
    let expected = Version::parse(version)
        .map_err(|_| AppError::new("runtimeVersionInvalid").value("version", version))?;
    let preferred_registry = fs::read_to_string(paths.cache_dir.join("npm.registry")).ok();
    let registries = ranked_install_registries(
        &client,
        npm_registries(),
        &expected,
        preferred_registry.as_deref().map(str::trim),
    )?;
    let seed_version = installed_version(paths).filter(|installed| {
        dsh_valid(paths, &paths.node_dir, &paths.dsh_dir, installed)
            && runtime_seed_has_space(paths)
    });
    let mut failures = Vec::new();
    let mut npm_total = NPM_PROCESS_TOTAL_TIMEOUT;
    let mut timed_out_once = false;
    for source in registries {
        controller.check()?;
        activity(
            notify,
            ActivityCode::DownloadingHarnessPackages,
            [
                ("version", version.to_owned()),
                ("source", display_source(&source.registry)),
                ("processed", "0".to_owned()),
            ],
        );
        let download_deadline = Instant::now()
            + Duration::from_secs(env_seconds("DSH_DESKTOP_DOWNLOAD_TIMEOUT_SECONDS", 600));
        let archive = match download_harness_tarball_with_retries(
            &client,
            &source,
            &paths.cache_dir,
            download_deadline,
            controller,
            notify,
        ) {
            Ok(archive) => archive,
            Err(error) if error.code == "deploymentCancelled" => return Err(error),
            Err(error) => {
                failures.push(format!(
                    "{}: {}",
                    display_source(&source.registry),
                    error
                        .safe_detail
                        .clone()
                        .unwrap_or_else(|| error.code.clone())
                ));
                log::warn!(
                    "skipping Harness {version} artifact from {}: {error}",
                    display_source(&source.registry)
                );
                continue;
            }
        };
        prepare_harness_staging(
            paths,
            staging,
            seed_version.as_deref(),
            version,
            controller,
            notify,
        )?;
        activity(
            notify,
            ActivityCode::InstallingHarness,
            [
                ("version", version.to_owned()),
                ("source", display_source(&source.registry)),
            ],
        );
        let mut command = harness_npm_install_command(paths, &archive, &source, staging);
        ensure_npm_cache(paths)?;
        let mut npm_activity = NpmInstallActivity::new(version, &source.registry);
        let install_result = run_logged(
            &mut command,
            &paths.install_log,
            ProcessTimeouts { total: npm_total },
            controller,
            |output, idle| npm_activity.observe(output, idle, notify),
        );
        match install_result {
            Ok(()) => {
                activity(
                    notify,
                    ActivityCode::ValidatingHarness,
                    [("version", version.to_owned())],
                );
                fix_spawn_helper(staging);
                if dsh_valid(paths, &paths.node_dir, staging, version) {
                    if let Err(error) = atomic_write(
                        &paths.cache_dir.join("npm.registry"),
                        format!("{}\n", source.registry).as_bytes(),
                    ) {
                        log::warn!("preferred npm registry could not be saved: {error}");
                    }
                    return Ok(staging.to_owned());
                }
                failures.push(format!(
                    "{}: runtimeValidationFailed",
                    display_source(&source.registry)
                ));
            }
            Err(error) if error.code == "deploymentCancelled" => return Err(error),
            // Registry fallback helps transport failures, but npm's local
            // peer-dependency solver is registry-independent, so a timed-out
            // source gets one reduced fallback attempt instead of another
            // full-resolution wait against an equivalent source.
            Err(error) if error.code == "processTimeout" => {
                failures.push(format!(
                    "{}: {}",
                    display_source(&source.registry),
                    error.code
                ));
                if timed_out_once {
                    break;
                }
                timed_out_once = true;
                npm_total = NPM_FALLBACK_PROCESS_TOTAL_TIMEOUT;
            }
            Err(error) => failures.push(format!(
                "{}: {}",
                display_source(&source.registry),
                error
                    .safe_detail
                    .clone()
                    .unwrap_or_else(|| error.code.clone())
            )),
        }
    }
    Err(AppError::new("installFailed")
        .value("log", paths.install_log.display())
        .detail(if failures.is_empty() {
            "no install source was available".to_owned()
        } else {
            failures.join("; ")
        }))
}

fn cleanup_failed_staging(staging: &Path, controller: &DeploymentController) -> AppResult<()> {
    if let Err(error) = remove_owned(staging) {
        controller.record_cleanup_error(error.clone());
        return Err(error);
    }
    Ok(())
}

fn harness_npm_install_command(
    paths: &ApplicationPaths,
    archive: &Path,
    source: &HarnessInstallSource,
    staging: &Path,
) -> Command {
    let npm = npm_cli(&paths.node_dir);
    let mut command = new_command(&paths.node_bin);
    command
        .arg(npm)
        .arg("install")
        .arg(archive)
        .args([
            "--no-save",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--package-lock=false",
            // Prefer-online keeps registry packuments fresh: the shared npm
            // cache can hold a packument fetched before a new release was
            // published, and npm's offline mode would keep resolving that
            // stale index and fail with ETARGET ("No matching version
            // found") even though the version exists on the registry.
            // Tarballs are still cached (they carry long max-age values).
            "--prefer-online",
            "--fetch-retries=2",
            "--fetch-retry-factor=2",
            "--fetch-retry-mintimeout=1000",
            "--fetch-retry-maxtimeout=10000",
            "--fetch-timeout=60000",
        ])
        .arg(format!("--cache={}", paths.cache_dir.join("npm").display()))
        .arg("--loglevel=silly");
    isolated_command(&mut command, paths);
    command
        .env("NPM_CONFIG_REGISTRY", &source.registry)
        .current_dir(staging);
    command
}

fn runtime_seed_has_space(paths: &ApplicationPaths) -> bool {
    let source_size = match owned_tree_size(&paths.dsh_dir) {
        Ok(size) => size,
        Err(error) => {
            log::warn!(
                "Harness runtime size could not be measured; using a fresh candidate: {error}"
            );
            return false;
        }
    };
    let available = match fs2::available_space(&paths.runtime_dir) {
        Ok(available) => available,
        Err(error) => {
            log::warn!("free disk space could not be measured; using a fresh candidate: {error}");
            return false;
        }
    };
    let required = runtime_seed_required(source_size);
    if available < required {
        log::warn!(
            "skipping Harness runtime reuse: {available} bytes available, {required} bytes required"
        );
        return false;
    }
    true
}

fn runtime_seed_required(source_size: u64) -> u64 {
    source_size.saturating_add(RUNTIME_COPY_RESERVE_BYTES)
}

#[cfg(test)]
fn has_runtime_seed_capacity(source_size: u64, available: u64) -> bool {
    available >= runtime_seed_required(source_size)
}

fn prepare_harness_staging(
    paths: &ApplicationPaths,
    staging: &Path,
    seed_version: Option<&str>,
    target_version: &str,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<()> {
    remove_owned(staging)?;
    let mut reused = false;
    if let Some(seed_version) = seed_version {
        activity(
            notify,
            ActivityCode::CopyingHarnessRuntime,
            [
                ("fromVersion", seed_version.to_owned()),
                ("version", target_version.to_owned()),
                ("processed", "0".to_owned()),
            ],
        );
        match copy_runtime_candidate(&paths.dsh_dir, staging, controller, |processed| {
            notify(DeploymentEvent::ActivityUpdate {
                values: BTreeMap::from([
                    ("fromVersion".to_owned(), seed_version.to_owned()),
                    ("version".to_owned(), target_version.to_owned()),
                    ("processed".to_owned(), processed.to_string()),
                ]),
            });
        }) {
            Ok(processed) => {
                log::info!(
                    "copied {processed} entries from Harness {seed_version} into the update candidate"
                );
                reused = true;
            }
            Err(error) if error.code == "deploymentCancelled" => return Err(error),
            Err(error) => {
                log::warn!(
                    "the installed Harness runtime could not seed the update candidate; using a fresh candidate: {error}"
                );
                remove_owned(staging)?;
            }
        }
    }

    if !reused {
        fs::create_dir(staging)?;
    }
    write_harness_manifest(staging, target_version)
}

fn write_harness_manifest(staging: &Path, target_version: &str) -> AppResult<()> {
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "name": "dsh-runtime",
        "private": true,
        "dependencies": {
            "@deepseek-ai/dsh": target_version,
        },
    }))?;
    atomic_write(&staging.join("package.json"), &manifest)
}

fn copy_runtime_candidate(
    source: &Path,
    destination: &Path,
    controller: &DeploymentController,
    mut progress: impl FnMut(u64),
) -> AppResult<u64> {
    let source_metadata = fs::symlink_metadata(source)?;
    if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
        return Err(AppError::new("runtimeSeedUnsafe"));
    }
    fs::create_dir(destination)?;
    let mut copied = 0_u64;
    let hidden_lock = source.join("node_modules/.package-lock.json");
    let result = copy_runtime_children(
        source,
        source,
        destination,
        &hidden_lock,
        controller,
        &mut copied,
        &mut progress,
    )
    .and_then(|()| {
        if hidden_lock.exists() && !hidden_lock.is_symlink() {
            let copied_lock = destination.join("node_modules/.package-lock.json");
            copy_runtime_file(&hidden_lock, &copied_lock, &mut copied, &mut progress)?;
            File::options()
                .write(true)
                .open(&copied_lock)?
                .set_modified(SystemTime::now())?;
        }
        fs::set_permissions(destination, source_metadata.permissions())?;
        progress(copied);
        Ok(copied)
    });
    match result {
        Ok(copied) => Ok(copied),
        Err(error) => {
            let _ = remove_owned(destination);
            Err(error)
        }
    }
}

fn copy_runtime_children(
    root: &Path,
    source: &Path,
    destination: &Path,
    hidden_lock: &Path,
    controller: &DeploymentController,
    copied: &mut u64,
    progress: &mut impl FnMut(u64),
) -> AppResult<()> {
    for entry in fs::read_dir(source)? {
        controller.check()?;
        let entry = entry?;
        let source_path = entry.path();
        if source_path == hidden_lock {
            continue;
        }
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)?;
            let directory = validate_runtime_symlink(root, &source_path, &target)?;
            create_runtime_symlink(&target, &destination_path, directory)?;
            include_runtime_copy_progress(copied, progress);
        } else if metadata.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_runtime_children(
                root,
                &source_path,
                &destination_path,
                hidden_lock,
                controller,
                copied,
                progress,
            )?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
            include_runtime_copy_progress(copied, progress);
        } else if metadata.is_file() {
            copy_runtime_file(&source_path, &destination_path, copied, progress)?;
        } else {
            return Err(AppError::new("runtimeSeedUnsafe").value("entry", source_path.display()));
        }
    }
    Ok(())
}

fn copy_runtime_file(
    source: &Path,
    destination: &Path,
    copied: &mut u64,
    progress: &mut impl FnMut(u64),
) -> AppResult<()> {
    fs::copy(source, destination)?;
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    include_runtime_copy_progress(copied, progress);
    Ok(())
}

fn include_runtime_copy_progress(copied: &mut u64, progress: &mut impl FnMut(u64)) {
    *copied = copied.saturating_add(1);
    if *copied == 1 || (*copied).is_multiple_of(256) {
        progress(*copied);
    }
}

fn validate_runtime_symlink(root: &Path, link: &Path, target: &Path) -> AppResult<bool> {
    if target.is_absolute() {
        return Err(AppError::new("runtimeSeedUnsafe").value("entry", link.display()));
    }
    let relative_parent = link
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .ok_or_else(|| AppError::new("runtimeSeedUnsafe").value("entry", link.display()))?;
    let mut normalized = PathBuf::new();
    for component in relative_parent.join(target).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir if normalized.pop() => {}
            _ => {
                return Err(AppError::new("runtimeSeedUnsafe").value("entry", link.display()));
            }
        }
    }
    let canonical_root = fs::canonicalize(root)?;
    let canonical_target = fs::canonicalize(root.join(normalized))?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err(AppError::new("runtimeSeedUnsafe").value("entry", link.display()));
    }
    Ok(fs::metadata(canonical_target)?.is_dir())
}

#[cfg(unix)]
fn create_runtime_symlink(target: &Path, link: &Path, _directory: bool) -> AppResult<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn create_runtime_symlink(target: &Path, link: &Path, directory: bool) -> AppResult<()> {
    if directory {
        std::os::windows::fs::symlink_dir(target, link)?;
    } else {
        std::os::windows::fs::symlink_file(target, link)?;
    }
    Ok(())
}

fn download_verified(
    urls: &[String],
    destination: &Path,
    checksum: &str,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<()> {
    if destination.exists() || destination.is_symlink() {
        if destination.is_file() && !destination.is_symlink() {
            let size = destination.metadata()?.len();
            if size <= MAX_NODE_ARCHIVE_BYTES && sha256(destination)? == checksum {
                notify(DeploymentEvent::Progress {
                    done: size,
                    total: Some(size),
                });
                return Ok(());
            }
        }
        remove_owned(destination)?;
    }
    let partial = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .and_then(|item| item.to_str())
            .unwrap_or_default()
    ));
    let _partial_cleanup = PartialDownload(partial.clone());
    let client = http_client()?;
    let deadline = Instant::now()
        + Duration::from_secs(env_seconds("DSH_DESKTOP_DOWNLOAD_TIMEOUT_SECONDS", 600));
    let mut errors = Vec::new();
    for attempt in 1..=2 {
        for url in urls {
            controller.check()?;
            if Instant::now() >= deadline {
                break;
            }
            match download_once(&client, url, &partial, deadline, controller, notify) {
                Ok(()) => {
                    let actual = sha256(&partial)?;
                    if actual == checksum {
                        fs::rename(&partial, destination)?;
                        return Ok(());
                    }
                    let _ = fs::remove_file(&partial);
                    errors.push(format!(
                        "{} attempt {attempt}: checksum mismatch",
                        display_source(url)
                    ));
                }
                Err(error) => {
                    let terminal = matches!(
                        error.code.as_str(),
                        "downloadTimedOut" | "downloadTooLarge" | "deploymentCancelled"
                    );
                    errors.push(format!(
                        "{} attempt {attempt}: {}",
                        display_source(url),
                        error
                            .safe_detail
                            .clone()
                            .unwrap_or_else(|| error.code.clone())
                    ));
                    if terminal {
                        let _ = remove_owned(&partial);
                        return Err(error);
                    }
                }
            }
        }
    }
    let _ = remove_owned(&partial);
    if Instant::now() >= deadline {
        return Err(AppError::new("downloadTimedOut"));
    }
    Err(AppError::new("downloadFailed").detail(errors.join("; ")))
}

fn download_once(
    client: &Client,
    url: &str,
    partial: &Path,
    deadline: Instant,
    controller: &DeploymentController,
    notify: &impl Fn(DeploymentEvent),
) -> AppResult<()> {
    validate_network_source(url)?;
    if Instant::now() >= deadline {
        return Err(AppError::new("downloadTimedOut"));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    remove_owned(partial)?;
    let mut response = client
        .get(url)
        .timeout(remaining)
        .send()
        .and_then(|item| item.error_for_status())
        .map_err(|error| {
            if error.is_timeout() || Instant::now() >= deadline {
                AppError::new("downloadTimedOut")
            } else {
                AppError::new("downloadFailed").detail(network::sanitize_detail(&error.to_string()))
            }
        })?;
    let total = response.content_length();
    if total.is_some_and(|size| size > MAX_NODE_ARCHIVE_BYTES) {
        return Err(AppError::new("downloadTooLarge"));
    }
    let mut file = File::create(partial)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut done = 0;
    loop {
        controller.check()?;
        if Instant::now() >= deadline {
            return Err(AppError::new("downloadTimedOut"));
        }
        let count = response.read(&mut buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut || Instant::now() >= deadline {
                AppError::new("downloadTimedOut")
            } else {
                AppError::io("downloadFailed", &error)
            }
        })?;
        if Instant::now() >= deadline {
            return Err(AppError::new("downloadTimedOut"));
        }
        if count == 0 {
            break;
        }
        if done + count as u64 > MAX_NODE_ARCHIVE_BYTES {
            return Err(AppError::new("downloadTooLarge"));
        }
        file.write_all(&buffer[..count])?;
        done += count as u64;
        notify(DeploymentEvent::Progress { done, total });
    }
    file.sync_all()?;
    Ok(())
}

fn extract_node(archive: &Path, destination: &Path) -> AppResult<()> {
    remove_owned(destination)?;
    fs::create_dir(destination)?;
    let result = if archive.extension().and_then(|value| value.to_str()) == Some("zip") {
        extract_zip(archive, destination)
    } else {
        extract_tar(archive, destination)
    };
    if let Err(error) = result {
        let _ = remove_owned(destination);
        return Err(error);
    }
    let children: Vec<_> = fs::read_dir(destination)?.collect::<Result<_, _>>()?;
    if children.len() != 1
        || !children[0].file_type()?.is_dir()
        || children[0].file_type()?.is_symlink()
    {
        return Err(AppError::new("nodeArchiveInvalid"));
    }
    let top = children[0].path();
    for child in fs::read_dir(&top)? {
        let child = child?;
        fs::rename(child.path(), destination.join(child.file_name()))?;
    }
    fs::remove_dir(top)?;
    Ok(())
}

fn extract_tar(archive: &Path, destination: &Path) -> AppResult<()> {
    let decoder = flate2::read::GzDecoder::new(File::open(archive)?);
    let mut bundle = tar::Archive::new(decoder);
    let mut extracted_bytes = 0;
    for item in bundle
        .entries()
        .map_err(|error| AppError::new("nodeArchiveInvalid").detail(error.to_string()))?
    {
        let mut item =
            item.map_err(|error| AppError::new("nodeArchiveInvalid").detail(error.to_string()))?;
        let entry_type = item.header().entry_type();
        if !(entry_type.is_file()
            || entry_type.is_dir()
            || entry_type.is_symlink()
            || entry_type.is_hard_link())
        {
            return Err(AppError::new("nodeArchiveUnsafe"));
        }
        if entry_type.is_file() {
            include_extracted_bytes(&mut extracted_bytes, item.size())?;
        }
        let path = item
            .path()
            .map_err(|error| AppError::new("nodeArchiveUnsafe").detail(error.to_string()))?
            .into_owned();
        validate_archive_path(&path)?;
        if let Some(link) = item
            .link_name()
            .map_err(|error| AppError::new("nodeArchiveUnsafe").detail(error.to_string()))?
        {
            validate_link(&path, &link)?;
        }
        if !item
            .unpack_in(destination)
            .map_err(|error| AppError::new("nodeArchiveUnsafe").detail(error.to_string()))?
        {
            return Err(AppError::new("nodeArchiveUnsafe"));
        }
    }
    Ok(())
}

fn extract_zip(archive: &Path, destination: &Path) -> AppResult<()> {
    let mut bundle = zip::ZipArchive::new(File::open(archive)?)
        .map_err(|error| AppError::new("nodeArchiveInvalid").detail(error.to_string()))?;
    let mut extracted_bytes = 0;
    for index in 0..bundle.len() {
        let mut item = bundle
            .by_index(index)
            .map_err(|error| AppError::new("nodeArchiveInvalid").detail(error.to_string()))?;
        let path = item
            .enclosed_name()
            .ok_or_else(|| AppError::new("nodeArchiveUnsafe"))?;
        validate_archive_path(&path)?;
        if item
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(AppError::new("nodeArchiveUnsafe"));
        }
        let output = destination.join(path);
        if item.is_dir() {
            fs::create_dir_all(&output)?;
        } else {
            include_extracted_bytes(&mut extracted_bytes, item.size())?;
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(&output)?;
            std::io::copy(&mut item, &mut file)?;
        }
    }
    Ok(())
}

fn include_extracted_bytes(total: &mut u64, size: u64) -> AppResult<()> {
    *total = total
        .checked_add(size)
        .ok_or_else(|| AppError::new("nodeArchiveTooLarge"))?;
    if *total > MAX_NODE_EXTRACTED_BYTES {
        return Err(AppError::new("nodeArchiveTooLarge"));
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> AppResult<()> {
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        Err(AppError::new("nodeArchiveUnsafe").value("entry", path.display()))
    } else {
        Ok(())
    }
}

fn validate_link(path: &Path, link: &Path) -> AppResult<()> {
    if link.is_absolute() {
        return Err(AppError::new("nodeArchiveUnsafe"));
    }
    let root = path
        .components()
        .next()
        .and_then(|part| match part {
            Component::Normal(value) => Some(value.to_owned()),
            _ => None,
        })
        .ok_or_else(|| AppError::new("nodeArchiveUnsafe"))?;
    let link_starts_at_root = link
        .components()
        .next()
        .is_some_and(|part| matches!(part, Component::Normal(value) if value == root));
    let combined = if link_starts_at_root {
        link.to_owned()
    } else {
        path.parent().unwrap_or_else(|| Path::new("")).join(link)
    };
    let mut normalized = Vec::new();
    for part in combined.components() {
        match part {
            Component::ParentDir => {
                if normalized.pop().is_none() {
                    return Err(AppError::new("nodeArchiveUnsafe"));
                }
            }
            Component::Normal(value) => normalized.push(value.to_owned()),
            Component::CurDir => {}
            _ => return Err(AppError::new("nodeArchiveUnsafe")),
        }
    }
    if normalized.first() == Some(&root) {
        Ok(())
    } else {
        Err(AppError::new("nodeArchiveUnsafe"))
    }
}

fn recover_interrupted(paths: &ApplicationPaths) -> AppResult<()> {
    for name in ["node", "dsh"] {
        let active = paths.runtime_dir.join(name);
        let previous = paths.runtime_dir.join(format!("{name}.previous"));
        if !active.exists() && previous.is_dir() && !previous.is_symlink() {
            fs::rename(previous, active)?;
        }
    }
    for entry in fs::read_dir(&paths.runtime_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("node.staging-")
            || name.starts_with("dsh.staging-")
            || name.contains(".failed-")
        {
            remove_owned(&entry.path())?;
        }
    }
    Ok(())
}

fn prune_oversized_npm_cache(paths: &ApplicationPaths) {
    if let Err(error) = prune_npm_cache_at(&paths.cache_dir, NPM_CACHE_PRUNE_THRESHOLD_BYTES) {
        log::warn!("oversized npm cache cleanup was skipped: {error}");
    }
}

fn prune_npm_cache_at(cache_dir: &Path, threshold: u64) -> AppResult<bool> {
    for legacy_marker in ["npm.last-used", "npm.last-prune-check"] {
        remove_owned(&cache_dir.join(legacy_marker))?;
    }
    cleanup_expired_npm_caches(cache_dir)?;

    let npm_cache = cache_dir.join("npm");
    if !npm_cache.exists() && !npm_cache.is_symlink() {
        return Ok(false);
    }
    if npm_cache.is_symlink() || !npm_cache.is_dir() {
        remove_owned(&npm_cache)?;
        return Ok(true);
    }
    if owned_tree_size(&npm_cache)? < threshold {
        return Ok(false);
    }

    let expired = cache_dir.join(format!("npm.expired-{}", Uuid::new_v4()));
    fs::rename(&npm_cache, &expired)?;
    // Deleting a large cache can fail transiently (locked files, scanners);
    // retry the removal on the next pruning pass instead of failing the
    // installation that triggered the prune.
    if let Err(error) = remove_owned(&expired) {
        log::warn!("expired npm cache will be retried later: {error}");
    }
    Ok(true)
}

fn cleanup_expired_npm_caches(cache_dir: &Path) -> AppResult<()> {
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("npm.expired-")
        {
            remove_owned(&entry.path())?;
        }
    }
    Ok(())
}

fn ensure_npm_cache(paths: &ApplicationPaths) -> AppResult<()> {
    // Cache pruning is best-effort: an oversized cache is a health issue,
    // not a reason to fail the installation that is about to use it.
    if let Err(error) = prune_npm_cache_at(&paths.cache_dir, NPM_CACHE_PRUNE_THRESHOLD_BYTES) {
        log::warn!("npm cache pruning was skipped: {error}");
    }
    let npm_cache = paths.cache_dir.join("npm");
    if npm_cache.exists() || npm_cache.is_symlink() {
        let metadata = fs::symlink_metadata(&npm_cache)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            remove_owned(&npm_cache)?;
        }
    }
    fs::create_dir_all(npm_cache)?;
    Ok(())
}

fn prune_old_node_archives(cache_dir: &Path, keep: &str) -> AppResult<()> {
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let node_archive = name.starts_with("node-v")
            && (name.ends_with(".tar.gz")
                || name.ends_with(".zip")
                || name.ends_with(".gzpart")
                || name.ends_with(".zippart"));
        if node_archive && name != keep {
            remove_owned(&entry.path())?;
        }
    }
    Ok(())
}

fn prune_stale_harness_archives(cache_dir: &Path) -> AppResult<()> {
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("harness.staging-") && name.ends_with(".tgz") {
            remove_owned(&entry.path())?;
        }
    }
    Ok(())
}

fn owned_tree_size(path: &Path) -> AppResult<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|error| {
            AppError::new("readDirectoryFailed")
                .detail(error.to_string())
                .value("path", path.display())
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() || metadata.file_type().is_symlink() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn recover_valid_previous(paths: &ApplicationPaths) -> AppResult<()> {
    let node_previous = paths.runtime_dir.join("node.previous");
    let dsh_previous = paths.runtime_dir.join("dsh.previous");
    let active_version = dsh_manifest_version(&paths.dsh_dir);
    if active_version
        .as_deref()
        .is_some_and(|version| runtime_pair_valid(paths, &paths.node_dir, &paths.dsh_dir, version))
    {
        if installed_version(paths).is_none() {
            let version = active_version.expect("validated active version");
            atomic_write(&paths.version_file, format!("{version}\n").as_bytes())?;
        }
        return Ok(());
    }

    let previous_version = dsh_manifest_version(&dsh_previous);
    let candidates = [
        (
            &node_previous,
            &paths.dsh_dir,
            active_version.as_deref(),
            true,
            false,
        ),
        (
            &paths.node_dir,
            &dsh_previous,
            previous_version.as_deref(),
            false,
            true,
        ),
        (
            &node_previous,
            &dsh_previous,
            previous_version.as_deref(),
            true,
            true,
        ),
    ];
    for (node, dsh, version, restore_node, restore_dsh) in candidates {
        let Some(version) = version else { continue };
        if !runtime_pair_valid(paths, node, dsh, version) {
            continue;
        }
        if restore_node {
            rollback_directory(&paths.node_dir, Some(&node_previous))?;
        }
        if restore_dsh {
            rollback_directory(&paths.dsh_dir, Some(&dsh_previous))?;
        }
        atomic_write(&paths.version_file, format!("{version}\n").as_bytes())?;
        return Ok(());
    }
    Ok(())
}

fn publish_directory(staging: &Path, active: &Path) -> AppResult<Option<PathBuf>> {
    let previous = active.with_file_name(format!(
        "{}.previous",
        active
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("runtime")
    ));
    let retired = previous.with_file_name(format!(
        "{}.failed-{}",
        previous
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("runtime.previous"),
        Uuid::new_v4()
    ));
    let had_previous = previous.exists() || previous.is_symlink();
    if had_previous {
        fs::rename(&previous, &retired)?;
    }
    let moved = active.exists();
    if moved && let Err(error) = fs::rename(active, &previous) {
        if had_previous {
            let _ = fs::rename(&retired, &previous);
        }
        return Err(error.into());
    }
    if let Err(error) = fs::rename(staging, active) {
        if moved && !active.exists() {
            let _ = fs::rename(&previous, active);
        }
        if had_previous && !previous.exists() {
            let _ = fs::rename(&retired, &previous);
        }
        return Err(error.into());
    }
    if had_previous && let Err(error) = remove_owned(&retired) {
        log::warn!("retired runtime backup will be cleaned during recovery: {error}");
    }
    Ok(moved.then_some(previous))
}

fn rollback_directory(active: &Path, previous: Option<&Path>) -> AppResult<()> {
    let failed = active.with_file_name(format!(
        "{}.failed-{}",
        active
            .file_name()
            .and_then(|item| item.to_str())
            .unwrap_or("runtime"),
        Uuid::new_v4()
    ));
    if active.exists() {
        fs::rename(active, &failed)?;
    }
    if let Some(previous) = previous
        && previous.exists()
    {
        fs::rename(previous, active)?;
    }
    remove_owned(&failed)
}

fn remove_owned(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn node_version(paths: &ApplicationPaths, node_dir: &Path) -> Option<String> {
    let executable = node_executable(node_dir);
    let mut command = new_command(executable);
    command.arg("--version");
    isolated_command(&mut command, paths);
    configure_process_group(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .strip_prefix('v')?
        .to_owned();
    Version::parse(&value).ok().map(|_| value)
}

fn dsh_valid(paths: &ApplicationPaths, node_dir: &Path, dsh_dir: &Path, version: &str) -> bool {
    if dsh_manifest_version(dsh_dir).as_deref() != Some(version) {
        return false;
    }
    let binary = dsh_dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js");
    let mut command = new_command(node_executable(node_dir));
    command.arg(binary).arg("--version");
    isolated_command(&mut command, paths);
    configure_process_group(&mut command);
    command.output().is_ok_and(|output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == version
    })
}

fn runtime_pair_valid(
    paths: &ApplicationPaths,
    node_dir: &Path,
    dsh_dir: &Path,
    version: &str,
) -> bool {
    node_version(paths, node_dir).is_some() && dsh_valid(paths, node_dir, dsh_dir, version)
}

fn dsh_manifest_version(dir: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(
        &fs::read(dir.join("node_modules/@deepseek-ai/dsh/package.json")).ok()?,
    )
    .ok()?;
    let version = value.get("version")?.as_str()?;
    Version::parse(version).ok().map(|_| version.to_owned())
}

#[derive(Debug, Clone, Copy)]
struct ProcessTimeouts {
    total: Duration,
}

fn run_logged(
    command: &mut Command,
    log_path: &Path,
    timeouts: ProcessTimeouts,
    controller: &DeploymentController,
    mut observe: impl FnMut(&str, Duration),
) -> AppResult<()> {
    trim_log_tail(log_path, INSTALL_LOG_MAX_BYTES)?;
    let result = run_logged_inner(command, log_path, timeouts, controller, &mut observe);
    trim_log_tail(log_path, INSTALL_LOG_MAX_BYTES)?;
    result
}

fn run_logged_inner(
    command: &mut Command,
    log_path: &Path,
    timeouts: ProcessTimeouts,
    controller: &DeploymentController,
    observe: &mut impl FnMut(&str, Duration),
) -> AppResult<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let output_start = log.metadata()?.len();
    let mut log_reader = File::open(log_path)?;
    log_reader.seek(SeekFrom::Start(output_start))?;
    command
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .stdin(Stdio::null());
    configure_process_group(command);
    let mut child = command.spawn()?;
    #[cfg(windows)]
    let job = WindowsProcessGuard::attach(&child)?;
    let started = Instant::now();
    let mut last_output = started;
    let mut last_observation = started;
    loop {
        let output = read_appended_log(&mut log_reader)?;
        if !output.is_empty() {
            last_output = Instant::now();
        }
        let now = Instant::now();
        if !output.is_empty() || now.duration_since(last_observation) >= Duration::from_secs(1) {
            observe(&output, now.duration_since(last_output));
            last_observation = now;
        }
        if controller.check().is_err() || now.duration_since(started) >= timeouts.total {
            #[cfg(unix)]
            stop_unix_command_tree(&mut child, controller)?;
            #[cfg(windows)]
            stop_windows_command_tree(&mut child, &job, controller)?;
            return Err(AppError::new(
                if controller.cancelled.load(Ordering::SeqCst) {
                    "deploymentCancelled"
                } else {
                    "processTimeout"
                },
            ));
        }
        #[cfg(windows)]
        job.observe()?;
        if let Some(status) = child.try_wait()? {
            let output = read_appended_log(&mut log_reader)?;
            if !output.is_empty() {
                observe(&output, Duration::ZERO);
            }
            #[cfg(unix)]
            stop_unix_command_tree(&mut child, controller)?;
            #[cfg(windows)]
            stop_windows_command_tree(&mut child, &job, controller)?;
            return if status.success() {
                Ok(())
            } else {
                Err(AppError::new("processFailed").value("status", status))
            };
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn read_appended_log(log: &mut File) -> AppResult<String> {
    let mut output = Vec::new();
    log.read_to_end(&mut output)?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

#[cfg(unix)]
fn stop_unix_command_tree(
    child: &mut std::process::Child,
    controller: &DeploymentController,
) -> AppResult<()> {
    let pid = child.id();
    if process_tree_alive(pid) {
        terminate_tree(pid, false);
    }
    let graceful_deadline = Instant::now() + Duration::from_secs(2);
    while process_tree_alive(pid) && Instant::now() < graceful_deadline {
        let _ = child.try_wait();
        thread::sleep(Duration::from_millis(50));
    }
    if process_tree_alive(pid) {
        terminate_tree(pid, true);
    }
    let _ = child.wait();
    let forced_deadline = Instant::now() + Duration::from_secs(5);
    while process_tree_alive(pid) && Instant::now() < forced_deadline {
        terminate_tree(pid, true);
        thread::sleep(Duration::from_millis(50));
    }
    if process_tree_alive(pid) {
        let error = AppError::new("serviceProcessTreeStillRunning").value("processId", pid);
        controller.record_cleanup_error(error.clone());
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn stop_windows_command_tree(
    child: &mut std::process::Child,
    job: &WindowsProcessGuard,
    controller: &DeploymentController,
) -> AppResult<()> {
    if let Err(error) = job.terminate() {
        controller.record_cleanup_error(error.clone());
        return Err(error);
    }
    let _ = child.wait();
    match job.wait_until_empty(Duration::from_secs(5)) {
        Ok(true) => Ok(()),
        Ok(false) => {
            let error = AppError::new("serviceProcessTreeStillRunning");
            controller.record_cleanup_error(error.clone());
            Err(error)
        }
        Err(error) => {
            controller.record_cleanup_error(error.clone());
            Err(error)
        }
    }
}

fn isolated_command(command: &mut Command, paths: &ApplicationPaths) {
    // Proxy variables are deliberately absent from this allowlist: they are
    // injected deterministically by the unified proxy configuration below.
    let allowed = [
        "COMSPEC",
        "LANG",
        "LC_ALL",
        "NODE_EXTRA_CA_CERTS",
        "PATH",
        "SSL_CERT_DIR",
        "SSL_CERT_FILE",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
    ];
    let values: Vec<_> = allowed
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| ((*key).to_owned(), value)))
        .collect();
    command
        .env_clear()
        .envs(values)
        .env("HOME", &paths.app_home)
        .env("USERPROFILE", &paths.app_home)
        .env(
            "NPM_CONFIG_USERCONFIG",
            paths.cache_dir.join("isolated-npmrc"),
        )
        .env("DSH_HOME", paths.cache_dir.join("validation-home"))
        .env("DSH_TELEMETRY_DISABLED", "1");
    network::apply_to_command(command);
}

#[cfg(unix)]
pub(crate) fn terminate_tree(pid: u32, force: bool) {
    unsafe {
        libc::kill(
            -(pid as i32),
            if force { libc::SIGKILL } else { libc::SIGTERM },
        );
    }
}
#[cfg(unix)]
pub(crate) fn process_tree_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let result = unsafe { libc::kill(-pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(windows)]
pub(crate) fn terminate_tree(pid: u32, _force: bool) {
    let mut command = new_command("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    configure_process_group(&mut command);
    let _ = command.output();
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) enum WindowsProcessGuard {
    Job(windows_sys::Win32::Foundation::HANDLE),
    Snapshot {
        processes: Mutex<std::collections::HashMap<u32, windows_sys::Win32::Foundation::HANDLE>>,
    },
}

#[cfg(windows)]
unsafe impl Send for WindowsProcessGuard {}
// Job handles are safe to query and terminate concurrently. The snapshot
// fallback protects its mutable handle map with a mutex.
#[cfg(windows)]
unsafe impl Sync for WindowsProcessGuard {}

#[cfg(windows)]
impl WindowsProcessGuard {
    pub(crate) fn attach(child: &std::process::Child) -> AppResult<Self> {
        match Self::attach_job(child) {
            Ok(handle) => Ok(Self::Job(handle)),
            Err(job_error) => {
                log::warn!(
                    "Windows Job Object unavailable; using direct process-tree cleanup: {job_error}"
                );
                Self::attach_snapshot(child)
            }
        }
    }

    pub(crate) fn attach_snapshot(child: &std::process::Child) -> AppResult<Self> {
        let root_handle = duplicate_process_handle(child)?;
        Ok(Self::Snapshot {
            processes: Mutex::new(std::iter::once((child.id(), root_handle)).collect()),
        })
    }

    fn attach_job(
        child: &std::process::Child,
    ) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use std::{mem::size_of, os::windows::io::AsRawHandle, ptr};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = WindowsOwnedHandle(handle);
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job.into_raw())
    }

    pub(crate) fn observe(&self) -> AppResult<()> {
        let Self::Snapshot { processes } = self else {
            return Ok(());
        };
        let entries = windows_process_entries()
            .map_err(|error| AppError::io("serviceProcessGuardFailed", &error))?;
        let mut processes = processes.lock().expect("Windows process guard poisoned");
        loop {
            let mut changed = false;
            for &(pid, parent_pid) in &entries {
                if pid != 0 && processes.contains_key(&parent_pid) && !processes.contains_key(&pid)
                {
                    match open_process_handle(pid) {
                        Ok(handle) => {
                            let handle = WindowsOwnedHandle(handle);
                            if !process_handle_running(handle.0).map_err(|error| {
                                AppError::io("serviceProcessGuardFailed", &error)
                            })? {
                                continue;
                            }
                            let confirmed_parent = windows_process_entries()?.into_iter().find_map(
                                |(candidate, parent)| (candidate == pid).then_some(parent),
                            );
                            if confirmed_parent
                                .is_some_and(|parent| processes.contains_key(&parent))
                            {
                                processes.insert(pid, handle.into_raw());
                                changed = true;
                            }
                        }
                        Err(error) if error.raw_os_error() == Some(87) => {}
                        Err(error) => {
                            return Err(AppError::io("serviceProcessGuardFailed", &error));
                        }
                    }
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    pub(crate) fn terminate(&self) -> AppResult<()> {
        use windows_sys::Win32::System::{
            JobObjects::TerminateJobObject, Threading::TerminateProcess,
        };

        match self {
            Self::Job(handle) => {
                if unsafe { TerminateJobObject(*handle, 1) } == 0 {
                    return Err(AppError::io(
                        "serviceProcessGuardFailed",
                        &std::io::Error::last_os_error(),
                    ));
                }
            }
            Self::Snapshot { processes } => {
                self.observe()?;
                for handle in processes
                    .lock()
                    .expect("Windows process guard poisoned")
                    .values()
                {
                    if process_handle_running(*handle)
                        .map_err(|error| AppError::io("serviceProcessGuardFailed", &error))?
                        && unsafe { TerminateProcess(*handle, 1) } == 0
                    {
                        return Err(AppError::io(
                            "serviceProcessGuardFailed",
                            &std::io::Error::last_os_error(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn wait_until_empty(&self, timeout: Duration) -> AppResult<bool> {
        use std::{mem::size_of, ptr};
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };

        let deadline = Instant::now() + timeout;
        loop {
            match self {
                Self::Job(handle) => {
                    let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                    if unsafe {
                        QueryInformationJobObject(
                            *handle,
                            JobObjectBasicAccountingInformation,
                            (&mut information as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION)
                                .cast(),
                            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                            ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(AppError::io(
                            "serviceProcessGuardFailed",
                            &std::io::Error::last_os_error(),
                        ));
                    }
                    if information.ActiveProcesses == 0 {
                        return Ok(true);
                    }
                }
                Self::Snapshot { processes } => {
                    self.observe()?;
                    self.terminate()?;
                    if processes
                        .lock()
                        .expect("Windows process guard poisoned")
                        .values()
                        .try_fold(true, |all_stopped, handle| {
                            process_handle_running(*handle).map(|running| all_stopped && !running)
                        })
                        .map_err(|error| AppError::io("serviceProcessGuardFailed", &error))?
                    {
                        return Ok(true);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::TerminateProcess};

        match self {
            Self::Job(handle) => unsafe {
                CloseHandle(*handle);
            },
            Self::Snapshot { processes } => {
                for handle in processes
                    .get_mut()
                    .expect("Windows process guard poisoned")
                    .values()
                {
                    unsafe {
                        if process_handle_running(*handle).unwrap_or(true) {
                            TerminateProcess(*handle, 1);
                        }
                        CloseHandle(*handle);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
struct WindowsOwnedHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsOwnedHandle {
    fn into_raw(self) -> windows_sys::Win32::Foundation::HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

#[cfg(windows)]
impl Drop for WindowsOwnedHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn duplicate_process_handle(
    child: &std::process::Child,
) -> AppResult<windows_sys::Win32::Foundation::HANDLE> {
    use std::{os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::{
        Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
        System::Threading::GetCurrentProcess,
    };

    let current = unsafe { GetCurrentProcess() };
    let mut duplicate = ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            current,
            child.as_raw_handle().cast(),
            current,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(AppError::io(
            "serviceProcessGuardFailed",
            &std::io::Error::last_os_error(),
        ));
    }
    Ok(duplicate)
}

#[cfg(windows)]
fn open_process_handle(pid: u32) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

#[cfg(windows)]
fn process_handle_running(handle: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };

    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_TIMEOUT => Ok(true),
        WAIT_OBJECT_0 => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        result => Err(std::io::Error::other(format!(
            "unexpected process wait result {result}"
        ))),
    }
}

#[cfg(windows)]
fn windows_process_entries() -> std::io::Result<Vec<(u32, u32)>> {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let snapshot = WindowsOwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut entries = Vec::new();
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(entries)
        } else {
            Err(error)
        };
    }
    loop {
        entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                return Err(error);
            }
            break;
        }
    }
    Ok(entries)
}

fn acquire_lock(file: &File, controller: &DeploymentController) -> AppResult<()> {
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    let contended = lock_contended_error().kind();
    loop {
        controller.check()?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == contended && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(200))
            }
            Err(error) if error.kind() == contended => {
                return Err(AppError::new("deploymentBusy"));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn http_client() -> AppResult<Client> {
    network::blocking_builder("dsh-desktop", &network::active())?
        .connect_timeout(Duration::from_secs(env_seconds(
            "DSH_DESKTOP_NETWORK_TIMEOUT_SECONDS",
            10,
        )))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| {
            AppError::new("networkClientFailed")
                .detail(network::sanitize_detail(&error.to_string()))
        })
}

/// Tests the given (possibly not yet persisted) proxy settings against every
/// configured Harness registry and reports each outcome. Validation and
/// client-construction problems are errors; per-registry outcomes — success
/// with the resolved latest version or a classified, sanitized failure — are
/// data, so the caller can show every source at once. A usable configuration
/// has at least one entry in `sources`.
pub fn test_proxy_connection(settings: &ProxySettings) -> AppResult<ProxyTestReport> {
    network::validate(settings)?;
    let client = network::blocking_builder("dsh-desktop-proxy-test", settings)?
        .connect_timeout(Duration::from_secs(env_seconds(
            "DSH_DESKTOP_NETWORK_TIMEOUT_SECONDS",
            10,
        )))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| {
            AppError::new("networkClientFailed")
                .detail(network::sanitize_detail(&error.to_string()))
        })?;
    let mut report = ProxyTestReport::default();
    for registry in npm_registries() {
        match query_registry_version(&client, &registry) {
            Ok(version) => report.sources.push(ProxyTestSource {
                source: display_source(&registry),
                version: version.to_string(),
            }),
            Err(error) => {
                // Query errors are already prefixed with their source; strip
                // it so the report (and any joined detail) never repeats the
                // registry address.
                let source = display_source(&registry);
                let detail = error
                    .safe_detail
                    .clone()
                    .unwrap_or_else(|| error.code.clone());
                let detail = detail
                    .strip_prefix(&format!("{source}: "))
                    .map(str::to_owned)
                    .unwrap_or(detail);
                report.failures.push(ProxyTestFailure {
                    source,
                    kind: error
                        .values
                        .get("kind")
                        .map(|kind| NetworkErrorKind::parse(kind))
                        .unwrap_or_default(),
                    detail,
                });
            }
        }
    }
    if report.sources.is_empty() && report.failures.is_empty() {
        return Err(AppError::new("proxyTestFailed").detail("no npm registry configured"));
    }
    Ok(report)
}
fn sha256(path: &Path) -> AppResult<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)
        .map_err(|error| AppError::io("checksumFailed", &error))?;
    Ok(hex::encode(digest.finalize()))
}
fn resolve_node_version() -> AppResult<String> {
    let value = std::env::var("DSH_DESKTOP_NODE_VERSION")
        .unwrap_or_else(|_| NODE_VERSION.into())
        .trim_start_matches('v')
        .to_owned();
    Version::parse(&value).map(|_| value.clone()).map_err(|_| {
        AppError::new("environmentInvalid")
            .value("variable", "DSH_DESKTOP_NODE_VERSION")
            .value("value", value)
    })
}
fn node_executable(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("node.exe")
    } else {
        dir.join("bin/node")
    }
}
fn npm_cli(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("node_modules/npm/bin/npm-cli.js")
    } else {
        dir.join("lib/node_modules/npm/bin/npm-cli.js")
    }
}
fn node_filename(version: &str) -> AppResult<String> {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        return Err(AppError::new("unsupportedPlatform"));
    };
    if cfg!(target_os = "macos") {
        Ok(format!("node-v{version}-darwin-{arch}.tar.gz"))
    } else if cfg!(windows) {
        Ok(format!("node-v{version}-win-{arch}.zip"))
    } else {
        Err(AppError::new("unsupportedPlatform"))
    }
}
fn node_checksum(filename: &str) -> AppResult<String> {
    if let Ok(value) = std::env::var("DSH_DESKTOP_NODE_SHA256") {
        return if value.len() == 64 && value.chars().all(|item| item.is_ascii_hexdigit()) {
            Ok(value.to_ascii_lowercase())
        } else {
            Err(AppError::new("environmentInvalid").value("variable", "DSH_DESKTOP_NODE_SHA256"))
        };
    }
    RELEASE_NODE_ASSETS
        .iter()
        .find_map(|(asset, checksum)| (*asset == filename).then(|| (*checksum).to_owned()))
        .ok_or_else(|| AppError::new("nodeChecksumMissing").value("filename", filename))
}
fn node_bases() -> Vec<String> {
    env_list("DSH_DESKTOP_NODE_BASES")
        .or_else(|| {
            std::env::var("DSH_DESKTOP_NODE_BASE")
                .ok()
                .map(|item| vec![item])
        })
        .unwrap_or_else(|| NODE_BASES.iter().map(ToString::to_string).collect())
}
fn npm_registries() -> Vec<String> {
    env_list("DSH_DESKTOP_NPM_REGISTRIES")
        .or_else(|| {
            std::env::var("DSH_DESKTOP_NPM_REGISTRY")
                .ok()
                .map(|item| vec![item])
        })
        .unwrap_or_else(|| NPM_REGISTRIES.iter().map(ToString::to_string).collect())
}
fn env_list(name: &str) -> Option<Vec<String>> {
    let values = std::env::var(name)
        .ok()?
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| item.trim_end_matches('/').to_owned())
        .collect::<Vec<_>>();
    (!values.is_empty()).then_some(values)
}
fn env_seconds(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
fn display_source(raw: &str) -> String {
    url::Url::parse(raw)
        .map(|mut value| {
            value.set_query(None);
            value.set_fragment(None);
            let _ = value.set_username("");
            let _ = value.set_password(None);
            value.to_string().trim_end_matches('/').to_owned()
        })
        .unwrap_or_else(|_| "<invalid source>".into())
}
fn validate_network_source(raw: &str) -> AppResult<url::Url> {
    let value = url::Url::parse(raw).map_err(|_| AppError::new("downloadSourceInvalid"))?;
    let local = matches!(value.host_str(), Some("127.0.0.1" | "localhost"));
    let transport_allowed = value.scheme() == "https" || (value.scheme() == "http" && local);
    if !transport_allowed
        || !value.username().is_empty()
        || value.password().is_some()
        || value.query().is_some()
        || value.fragment().is_some()
    {
        return Err(AppError::new("downloadSourceInvalid"));
    }
    Ok(value)
}
fn activity<const N: usize>(
    notify: &impl Fn(DeploymentEvent),
    code: ActivityCode,
    values: [(&str, String); N],
) {
    notify(DeploymentEvent::Activity {
        code,
        values: values
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    });
}

fn fix_spawn_helper(_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in walkdir::WalkDir::new(_dir.join("node_modules/node-pty/prebuilds"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() == "spawn-helper")
        {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, net::TcpListener, thread};

    #[cfg(unix)]
    fn write_fake_runtime(paths: &ApplicationPaths, directory: &Path, version: &str) {
        use std::os::unix::fs::PermissionsExt;

        let node = paths.node_dir.join("bin/node");
        fs::create_dir_all(node.parent().unwrap()).unwrap();
        fs::write(
            &node,
            b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo v24.19.0; exit 0; fi\nscript=\"$1\"\nshift\nexec \"$script\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();

        let package = directory.join("node_modules/@deepseek-ai/dsh");
        let binary = package.join("lib/bin.js");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(
            package.join("package.json"),
            format!("{{\"version\":\"{version}\"}}\n"),
        )
        .unwrap();
        fs::write(&binary, format!("#!/bin/sh\necho {version}\n")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn node_assets_are_pinned_for_release_targets() {
        for (filename, _) in RELEASE_NODE_ASSETS {
            assert_eq!(node_checksum(filename).unwrap().len(), 64);
        }
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(validate_archive_path(Path::new("node/../../secret")).is_err());
        assert!(validate_link(Path::new("node/bin/npm"), Path::new("../../outside")).is_err());
    }

    #[test]
    fn extracted_archive_size_is_bounded() {
        let mut total = MAX_NODE_EXTRACTED_BYTES;
        assert_eq!(
            include_extracted_bytes(&mut total, 1).unwrap_err().code,
            "nodeArchiveTooLarge"
        );
    }

    #[test]
    fn network_sources_require_https_except_for_loopback_tests() {
        assert!(validate_network_source("https://registry.npmjs.org").is_ok());
        assert!(validate_network_source("http://127.0.0.1:8123").is_ok());
        assert!(validate_network_source("http://registry.example.test").is_err());
        assert!(validate_network_source("https://token@example.test").is_err());
    }

    #[test]
    fn registry_latest_must_exist_in_the_full_install_index() {
        let stale = json_response(
            r#"{"dist-tags":{"latest":"0.1.1-rc.2"},"versions":{"0.1.1-rc.1":{"version":"0.1.1-rc.1","dist":{"tarball":"https://registry.example.test/dsh-0.1.1-rc.1.tgz","integrity":"sha512-test"}}}}"#,
        );
        let (registry, server) = serve_responses(vec![stale]);

        let error = query_registry_version(&http_client().unwrap(), &registry).unwrap_err();
        server.join().unwrap();

        assert_eq!(error.code, "versionQueryFailed");
        assert!(
            error
                .safe_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("absent from the install version index"))
        );
    }

    #[test]
    fn registry_latest_is_accepted_from_the_full_install_index() {
        let metadata = harness_packument("0.1.0-rc.7");
        let (registry, server) = serve_responses(vec![metadata]);

        let version = query_registry_version(&http_client().unwrap(), &registry).unwrap();
        server.join().unwrap();

        assert_eq!(version, Version::parse("0.1.0-rc.7").unwrap());
    }

    #[test]
    fn harness_tarball_must_match_the_published_integrity() {
        let body = b"verified-harness-archive";
        let integrity = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(body));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let (tarball, server) = serve_responses(vec![response]);
        let source = HarnessInstallSource {
            registry: tarball.clone(),
            tarball,
            integrity,
        };
        let temp = tempfile::tempdir().unwrap();

        let archive = download_harness_tarball(
            &http_client().unwrap(),
            &source,
            temp.path(),
            Instant::now() + Duration::from_secs(2),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(&archive).unwrap(), body);
        assert_eq!(
            archive.extension().and_then(|value| value.to_str()),
            Some("tgz")
        );
    }

    #[test]
    fn corrupted_harness_tarball_is_rejected_and_removed() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt";
        let (tarball, server) = serve_responses(vec![response.into()]);
        let source = HarnessInstallSource {
            registry: tarball.clone(),
            tarball,
            integrity: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        };
        let temp = tempfile::tempdir().unwrap();

        let error = download_harness_tarball(
            &http_client().unwrap(),
            &source,
            temp.path(),
            Instant::now() + Duration::from_secs(2),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.code, "checksumFailed");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn harness_tarball_download_retries_transient_failures() {
        let body = b"verified-after-retry";
        let integrity = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(body));
        let success = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let (tarball, server) = serve_responses(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            success,
        ]);
        let source = HarnessInstallSource {
            registry: tarball.clone(),
            tarball,
            integrity,
        };
        let temp = tempfile::tempdir().unwrap();

        let archive = download_harness_tarball_with_retries(
            &http_client().unwrap(),
            &source,
            temp.path(),
            Instant::now() + Duration::from_secs(2),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(archive).unwrap(), body);
    }

    #[test]
    fn harness_tarball_download_uses_its_explicit_deadline() {
        let (tarball, server) = serve_once(|mut stream| {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&[1_u8; 1024]);
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(&[1_u8; 1024]);
        });
        let source = HarnessInstallSource {
            registry: tarball.clone(),
            tarball,
            integrity: base64::engine::general_purpose::STANDARD.encode([0_u8; 64]),
        };
        let temp = tempfile::tempdir().unwrap();

        let error = download_harness_tarball(
            &http_client().unwrap(),
            &source,
            temp.path(),
            Instant::now() + Duration::from_millis(20),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.code, "downloadTimedOut");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn exact_install_sources_are_ranked_by_observed_latency() {
        let metadata = harness_packument("0.1.0-rc.7");
        let slow_response = metadata.clone();
        let (slow, slow_server) = serve_once(move |mut stream| {
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(40));
            stream.write_all(slow_response.as_bytes()).unwrap();
        });
        let (fast, fast_server) = serve_once(move |mut stream| {
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(metadata.as_bytes()).unwrap();
        });

        let ranked = ranked_install_registries(
            &http_client().unwrap(),
            vec![slow.clone(), fast.clone()],
            &Version::parse("0.1.0-rc.7").unwrap(),
            None,
        )
        .unwrap();
        slow_server.join().unwrap();
        fast_server.join().unwrap();

        assert_eq!(
            ranked
                .into_iter()
                .map(|source| source.registry)
                .collect::<Vec<_>>(),
            vec![fast, slow]
        );
    }

    #[test]
    fn last_successful_install_source_is_kept_for_cache_reuse() {
        let metadata = harness_packument("0.1.0-rc.7");
        let slow_response = metadata.clone();
        let (slow, slow_server) = serve_once(move |mut stream| {
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(40));
            stream.write_all(slow_response.as_bytes()).unwrap();
        });
        let (fast, fast_server) = serve_once(move |mut stream| {
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(metadata.as_bytes()).unwrap();
        });

        let ranked = ranked_install_registries(
            &http_client().unwrap(),
            vec![slow.clone(), fast.clone()],
            &Version::parse("0.1.0-rc.7").unwrap(),
            Some(&slow),
        )
        .unwrap();
        slow_server.join().unwrap();
        fast_server.join().unwrap();

        assert_eq!(
            ranked
                .into_iter()
                .map(|source| source.registry)
                .collect::<Vec<_>>(),
            vec![slow, fast]
        );
    }

    #[test]
    fn invalid_install_source_is_skipped_without_aborting_the_ranking() {
        let metadata = harness_packument("0.1.0-rc.7");
        let (valid, server) = serve_responses(vec![metadata]);

        let ranked = ranked_install_registries(
            &http_client().unwrap(),
            vec!["ftp://unreachable.invalid".to_owned(), valid.clone()],
            &Version::parse("0.1.0-rc.7").unwrap(),
            None,
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(
            ranked
                .into_iter()
                .map(|source| source.registry)
                .collect::<Vec<_>>(),
            vec![valid]
        );
    }

    #[test]
    fn harness_npm_install_revalidates_registry_packuments() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let source = HarnessInstallSource {
            registry: "https://registry.npmjs.org".to_owned(),
            tarball: "https://registry.npmjs.org/archive.tgz".to_owned(),
            integrity: String::new(),
        };
        let staging = paths.runtime_dir.join("dsh.staging-test");
        fs::create_dir(&staging).unwrap();

        let command =
            harness_npm_install_command(&paths, Path::new("harness.tgz"), &source, &staging);
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.iter().any(|argument| argument == "--prefer-online"));
        assert!(
            !args
                .iter()
                .any(|argument| argument.contains("prefer-offline"))
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == "NPM_CONFIG_REGISTRY" && value == Some(std::ffi::OsStr::new(&source.registry))
        }));
    }

    #[test]
    fn npm_output_reports_real_install_phases_and_waiting() {
        let events = RefCell::new(Vec::new());
        let notify = |event| events.borrow_mut().push(event);
        let mut activity = NpmInstallActivity::new("0.1.0-rc.7", "https://registry.npmjs.org");

        activity.observe(
            "14 silly fetch manifest @deepseek-ai/dsh@0.1.0-rc.7\n",
            Duration::ZERO,
            &notify,
        );
        activity.observe(
            "20 http fetch GET 200 https://registry.npmjs.org/a/-/a-1.0.0.tgz\n",
            Duration::ZERO,
            &notify,
        );
        activity.observe("21 silly ADD node_modules/a\n", Duration::ZERO, &notify);
        activity.observe("", NPM_WAITING_AFTER, &notify);

        let events = events.into_inner();
        assert!(matches!(
            events.first(),
            Some(DeploymentEvent::Activity {
                code: ActivityCode::ResolvingHarnessDependencies,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            DeploymentEvent::Activity {
                code: ActivityCode::DownloadingHarnessPackages,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DeploymentEvent::Activity {
                code: ActivityCode::WritingHarnessRuntime,
                ..
            }
        )));
        assert!(matches!(
            events.last(),
            Some(DeploymentEvent::ActivityUpdate { values })
                if values.get("status").map(String::as_str) == Some("waiting")
        ));
    }

    #[test]
    fn runtime_candidate_copy_is_independent_and_keeps_the_hidden_lock() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("active");
        let destination = temp.path().join("candidate");
        let package = source.join("node_modules/example");
        fs::create_dir_all(&package).unwrap();
        fs::write(source.join("package.json"), b"{\"private\":true}\n").unwrap();
        fs::write(package.join("value"), b"active").unwrap();
        fs::write(
            source.join("node_modules/.package-lock.json"),
            b"{\"lockfileVersion\":3}\n",
        )
        .unwrap();
        let progress = RefCell::new(Vec::new());

        let copied = copy_runtime_candidate(
            &source,
            &destination,
            &DeploymentController::default(),
            |value| progress.borrow_mut().push(value),
        )
        .unwrap();

        assert!(copied >= 5);
        assert_eq!(
            fs::read(destination.join("node_modules/.package-lock.json")).unwrap(),
            b"{\"lockfileVersion\":3}\n"
        );
        let lock_modified = fs::metadata(destination.join("node_modules/.package-lock.json"))
            .unwrap()
            .modified()
            .unwrap();
        for directory in [
            destination.join("node_modules"),
            destination.join("node_modules/example"),
        ] {
            assert!(lock_modified >= fs::metadata(directory).unwrap().modified().unwrap());
        }
        fs::write(destination.join("node_modules/example/value"), b"candidate").unwrap();
        assert_eq!(
            fs::read(source.join("node_modules/example/value")).unwrap(),
            b"active"
        );
        assert_eq!(progress.borrow().last().copied(), Some(copied));
    }

    #[test]
    fn reused_runtime_manifest_tracks_the_requested_version() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::create_dir_all(paths.dsh_dir.join("node_modules/example")).unwrap();
        fs::write(
            paths.dsh_dir.join("package.json"),
            br#"{"name":"dsh-runtime","private":true,"dependencies":{"@deepseek-ai/dsh":"0.1.1-rc.1"}}"#,
        )
        .unwrap();
        let staging = paths.runtime_dir.join("dsh.staging-test");

        prepare_harness_staging(
            &paths,
            &staging,
            Some("0.1.1-rc.1"),
            "0.1.1-rc.2",
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap();

        let manifest: Value =
            serde_json::from_slice(&fs::read(staging.join("package.json")).unwrap()).unwrap();
        assert_eq!(manifest["dependencies"]["@deepseek-ai/dsh"], "0.1.1-rc.2");
    }

    #[test]
    fn cancelled_runtime_candidate_copy_removes_its_partial_directory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("active");
        let destination = temp.path().join("candidate");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("value"), b"active").unwrap();
        let controller = DeploymentController::default();
        controller.cancel();

        let error = copy_runtime_candidate(&source, &destination, &controller, |_| {}).unwrap_err();

        assert_eq!(error.code, "deploymentCancelled");
        assert!(!destination.exists());
        assert_eq!(fs::read(source.join("value")).unwrap(), b"active");
    }

    #[test]
    fn failed_staging_is_removed_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("dsh.staging-failed");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("partial"), b"candidate").unwrap();
        let controller = DeploymentController::default();

        cleanup_failed_staging(&staging, &controller).unwrap();

        assert!(!staging.exists());
        assert!(controller.cleanup_error().is_none());
    }

    #[test]
    fn startup_recovery_removes_every_stale_candidate_and_failed_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        for name in [
            "dsh.staging-one",
            "dsh.staging-two",
            "node.staging-one",
            "dsh.failed-one",
        ] {
            fs::create_dir(paths.runtime_dir.join(name)).unwrap();
        }

        recover_interrupted(&paths).unwrap();

        for name in [
            "dsh.staging-one",
            "dsh.staging-two",
            "node.staging-one",
            "dsh.failed-one",
        ] {
            assert!(!paths.runtime_dir.join(name).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_recovery_removes_a_candidate_link_without_following_it() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"user-data").unwrap();
        symlink(&outside, paths.runtime_dir.join("dsh.staging-link")).unwrap();

        recover_interrupted(&paths).unwrap();

        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"user-data");
        assert!(!paths.runtime_dir.join("dsh.staging-link").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_runtime_link_falls_back_to_a_fresh_candidate() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"user-data").unwrap();
        fs::create_dir_all(paths.dsh_dir.join("node_modules")).unwrap();
        symlink(&outside, paths.dsh_dir.join("node_modules/external")).unwrap();
        let staging = paths.runtime_dir.join("dsh.staging-test");

        prepare_harness_staging(
            &paths,
            &staging,
            Some("0.1.0-rc.7"),
            "0.1.1-rc.1",
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap();

        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"user-data");
        assert!(!staging.join("node_modules/external").exists());
        let manifest: Value =
            serde_json::from_slice(&fs::read(staging.join("package.json")).unwrap()).unwrap();
        assert_eq!(manifest["dependencies"]["@deepseek-ai/dsh"], "0.1.1-rc.1");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_candidate_copy_preserves_safe_relative_links() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("active");
        let destination = temp.path().join("candidate");
        fs::create_dir_all(source.join("node_modules/.bin")).unwrap();
        fs::create_dir_all(source.join("node_modules/example/bin")).unwrap();
        fs::write(source.join("node_modules/example/bin/cli.js"), b"cli").unwrap();
        symlink(
            "../example/bin/cli.js",
            source.join("node_modules/.bin/example"),
        )
        .unwrap();

        copy_runtime_candidate(
            &source,
            &destination,
            &DeploymentController::default(),
            |_| {},
        )
        .unwrap();

        let copied_link = destination.join("node_modules/.bin/example");
        assert!(
            fs::symlink_metadata(&copied_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(copied_link).unwrap(),
            PathBuf::from("../example/bin/cli.js")
        );
    }

    #[test]
    fn run_logged_streams_appended_output_to_the_observer() {
        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("install.log");
        File::create(&log_path)
            .unwrap()
            .set_len(INSTALL_LOG_MAX_BYTES + 1024)
            .unwrap();
        let mut command = new_command(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "runtime::tests::run_logged_output_helper",
                "--nocapture",
            ])
            .env("DSH_RUN_LOGGED_OUTPUT_HELPER", "1");
        let observed = RefCell::new(Vec::new());

        run_logged(
            &mut command,
            &log_path,
            ProcessTimeouts {
                total: Duration::from_secs(10),
            },
            &DeploymentController::default(),
            |output, _| {
                if !output.is_empty() {
                    observed.borrow_mut().push(output.to_owned());
                }
            },
        )
        .unwrap();

        let observed = observed.into_inner();
        assert!(observed.iter().any(|chunk| chunk.contains("first-output")));
        assert!(observed.iter().any(|chunk| chunk.contains("second-output")));
        assert!(fs::metadata(log_path).unwrap().len() <= INSTALL_LOG_MAX_BYTES);
    }

    #[test]
    fn run_logged_output_helper() {
        if std::env::var_os("DSH_RUN_LOGGED_OUTPUT_HELPER").is_none() {
            return;
        }
        println!("first-output");
        std::io::stdout().flush().unwrap();
        thread::sleep(Duration::from_millis(250));
        println!("second-output");
        std::io::stdout().flush().unwrap();
    }

    #[test]
    fn npm_cache_is_pruned_as_soon_as_it_reaches_the_limit() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let npm = cache.join("npm");
        fs::create_dir_all(&npm).unwrap();
        fs::write(npm.join("cached-package"), [0_u8; 8]).unwrap();

        assert!(!prune_npm_cache_at(&cache, 9).unwrap());
        assert!(npm.exists());
        assert!(prune_npm_cache_at(&cache, 8).unwrap());
        assert!(!npm.exists());
    }

    #[test]
    fn npm_cache_preparation_removes_interrupted_cleanup_before_reuse() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let expired = paths.cache_dir.join("npm.expired-interrupted");
        fs::create_dir(&expired).unwrap();
        fs::write(expired.join("cached-package"), b"old-cache").unwrap();
        fs::write(paths.cache_dir.join("npm.last-used"), b"legacy-marker").unwrap();

        ensure_npm_cache(&paths).unwrap();

        assert!(!expired.exists());
        assert!(!paths.cache_dir.join("npm.last-used").exists());
        assert!(paths.cache_dir.join("npm").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn npm_cache_pruning_never_follows_links_outside_the_cache() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let npm = cache.join("npm");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&npm).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"user-data").unwrap();
        symlink(&outside, npm.join("external-link")).unwrap();

        assert!(prune_npm_cache_at(&cache, 1).unwrap());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"user-data");
    }

    #[cfg(unix)]
    #[test]
    fn npm_cache_root_link_is_replaced_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"user-data").unwrap();
        symlink(&outside, paths.cache_dir.join("npm")).unwrap();

        ensure_npm_cache(&paths).unwrap();

        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"user-data");
        let metadata = fs::symlink_metadata(paths.cache_dir.join("npm")).unwrap();
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
    }

    #[test]
    fn old_node_archives_and_partial_downloads_are_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        let keep = "node-v24.19.0-win-x64.zip";
        fs::write(cache.join(keep), b"current").unwrap();
        fs::write(cache.join("node-v22.0.0-win-x64.zip"), b"old").unwrap();
        fs::write(cache.join("node-v22.0.0-darwin-x64.tar.gzpart"), b"partial").unwrap();
        fs::write(cache.join("unrelated"), b"keep").unwrap();

        prune_old_node_archives(cache, keep).unwrap();

        assert!(cache.join(keep).exists());
        assert!(!cache.join("node-v22.0.0-win-x64.zip").exists());
        assert!(!cache.join("node-v22.0.0-darwin-x64.tar.gzpart").exists());
        assert!(cache.join("unrelated").exists());
    }

    #[test]
    fn interrupted_harness_tarball_is_pruned_before_the_next_update() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path();
        fs::write(cache.join("harness.staging-abcd.tgz"), b"partial").unwrap();
        fs::write(cache.join("harness-release.tgz"), b"unrelated").unwrap();

        prune_stale_harness_archives(cache).unwrap();

        assert!(!cache.join("harness.staging-abcd.tgz").exists());
        assert!(cache.join("harness-release.tgz").exists());
    }

    #[test]
    fn runtime_copy_requires_space_for_the_copy_and_install_reserve() {
        let source = 256 * 1024 * 1024;
        let required = source + RUNTIME_COPY_RESERVE_BYTES;

        assert!(!has_runtime_seed_capacity(source, required - 1));
        assert!(has_runtime_seed_capacity(source, required));
    }

    #[test]
    fn failed_publication_can_restore_previous_directory() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("runtime");
        let staging = temp.path().join("runtime.staging-test");
        fs::create_dir(&active).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(active.join("value"), "old").unwrap();
        fs::write(staging.join("value"), "candidate").unwrap();
        let previous = publish_directory(&staging, &active).unwrap();
        assert_eq!(
            fs::read_to_string(active.join("value")).unwrap(),
            "candidate"
        );
        rollback_directory(&active, previous.as_deref()).unwrap();
        assert_eq!(fs::read_to_string(active.join("value")).unwrap(), "old");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_harness_update_is_persisted_and_activated_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        write_fake_runtime(&paths, &paths.dsh_dir, "0.1.0");
        fs::write(&paths.version_file, b"0.1.0\n").unwrap();
        write_fake_runtime(&paths, &paths.pending_dsh_dir, "0.2.0");
        atomic_write(
            &paths.pending_harness_update_file,
            br#"{"version":"0.2.0"}"#,
        )
        .unwrap();

        assert_eq!(
            prepared_harness_update(&paths).unwrap(),
            Some("0.2.0".into())
        );
        let activated =
            activate_prepared_harness_update(&paths, &DeploymentController::default(), |_| {})
                .unwrap();

        assert_eq!(activated, "0.2.0");
        assert_eq!(installed_version(&paths).as_deref(), Some("0.2.0"));
        assert!(!paths.pending_dsh_dir.exists());
        assert!(!paths.pending_harness_update_file.exists());
        assert_eq!(
            dsh_manifest_version(&paths.runtime_dir.join("dsh.previous")).as_deref(),
            Some("0.1.0")
        );
    }

    #[test]
    fn incomplete_prepared_harness_update_is_never_reported_as_ready() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        atomic_write(
            &paths.pending_harness_update_file,
            br#"{"version":"0.2.0"}"#,
        )
        .unwrap();

        assert_eq!(
            prepared_harness_update(&paths).unwrap_err().code,
            "preparedHarnessUpdateInvalid"
        );
        assert_eq!(recover_prepared_harness_update(&paths).unwrap(), None);
        assert!(!paths.pending_harness_update_file.exists());
    }

    #[test]
    fn orphaned_prepared_candidate_is_cleaned_during_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::create_dir(&paths.pending_dsh_dir).unwrap();

        assert_eq!(recover_prepared_harness_update(&paths).unwrap(), None);
        assert!(!paths.pending_dsh_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_candidate_cleanup_never_follows_a_directory_link() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("user-data"), b"preserve").unwrap();
        symlink(&outside, &paths.pending_dsh_dir).unwrap();

        assert_eq!(recover_prepared_harness_update(&paths).unwrap(), None);
        assert!(!paths.pending_dsh_dir.is_symlink());
        assert_eq!(fs::read(outside.join("user-data")).unwrap(), b"preserve");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_candidate_never_downgrades_an_equal_or_newer_active_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        write_fake_runtime(&paths, &paths.dsh_dir, "0.2.0");
        fs::write(&paths.version_file, b"0.2.0\n").unwrap();
        write_fake_runtime(&paths, &paths.pending_dsh_dir, "0.1.0");
        atomic_write(
            &paths.pending_harness_update_file,
            br#"{"version":"0.1.0"}"#,
        )
        .unwrap();

        assert_eq!(recover_prepared_harness_update(&paths).unwrap(), None);
        assert!(!paths.pending_dsh_dir.exists());
        assert!(!paths.pending_harness_update_file.exists());
        assert_eq!(installed_version(&paths).as_deref(), Some("0.2.0"));
    }

    #[test]
    fn failed_publication_preserves_active_and_previous_runtimes() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("dsh");
        let previous = temp.path().join("dsh.previous");
        fs::create_dir(&active).unwrap();
        fs::create_dir(&previous).unwrap();
        fs::write(active.join("value"), "active").unwrap();
        fs::write(previous.join("value"), "previous").unwrap();

        assert!(publish_directory(&temp.path().join("missing"), &active).is_err());
        assert_eq!(fs::read_to_string(active.join("value")).unwrap(), "active");
        assert_eq!(
            fs::read_to_string(previous.join("value")).unwrap(),
            "previous"
        );
    }

    #[test]
    fn repeated_publication_keeps_only_active_and_one_previous_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("dsh");
        fs::create_dir(&active).unwrap();
        fs::write(active.join("version"), "one").unwrap();

        for version in ["two", "three", "four"] {
            let staging = temp.path().join(format!("dsh.staging-{version}"));
            fs::create_dir(&staging).unwrap();
            fs::write(staging.join("version"), version).unwrap();
            publish_directory(&staging, &active).unwrap();
        }

        assert_eq!(fs::read_to_string(active.join("version")).unwrap(), "four");
        assert_eq!(
            fs::read_to_string(temp.path().join("dsh.previous/version")).unwrap(),
            "three"
        );
        let runtime_directories = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count();
        assert_eq!(runtime_directories, 2);
    }

    #[test]
    fn download_rejects_an_oversized_content_length() {
        let (url, server) = serve_once(|mut stream| {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_NODE_ARCHIVE_BYTES + 1
            );
            let _ = stream.flush();
            // Keep the fixture alive long enough for the client to return the
            // response headers. Closing immediately can race reqwest into an
            // incomplete-body error before download_once checks Content-Length.
            thread::sleep(Duration::from_millis(20));
        });
        let temp = tempfile::tempdir().unwrap();
        let error = download_once(
            &http_client().unwrap(),
            &url,
            &temp.path().join("archive.part"),
            Instant::now() + Duration::from_secs(2),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.code, "downloadTooLarge");
    }

    #[test]
    fn download_deadline_is_enforced_while_streaming() {
        let (url, server) = serve_once(|mut stream| {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&[1u8; 1024]);
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(&[1u8; 1024]);
        });
        let temp = tempfile::tempdir().unwrap();
        let error = download_once(
            &http_client().unwrap(),
            &url,
            &temp.path().join("archive.part"),
            Instant::now() + Duration::from_millis(20),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.code, "downloadTimedOut");
    }

    #[test]
    fn oversized_cached_archive_is_not_reused() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("archive");
        File::create(&archive)
            .unwrap()
            .set_len(MAX_NODE_ARCHIVE_BYTES + 1)
            .unwrap();
        let error = download_verified(
            &[],
            &archive,
            &"0".repeat(64),
            &DeploymentController::default(),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(error.code, "downloadFailed");
        assert!(!archive.exists());
    }

    fn serve_once(
        handler: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handler(stream);
        });
        (format!("http://{address}/archive"), server)
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn harness_packument(version: &str) -> String {
        let integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode([0_u8; 64])
        );
        json_response(
            &serde_json::json!({
                "dist-tags": { "latest": version },
                "versions": {
                    version: {
                        "version": version,
                        "dist": {
                            "tarball": format!("https://registry.example.test/dsh-{version}.tgz"),
                            "integrity": integrity,
                        },
                    },
                },
            })
            .to_string(),
        )
    }

    fn serve_responses(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{address}"), server)
    }

    // -----------------------------------------------------------------------
    // Multi-registry and proxy-behavior tests. `npm_registries` and the
    // system proxy mode read process environment variables, so these run in
    // a child copy of the test binary with a cleared, controlled environment
    // instead of mutating the shared environment of the test runner.
    // -----------------------------------------------------------------------

    fn closed_loopback_registry() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}")
    }

    fn spawn_version_query_child(
        registries: &str,
        extra: &[(&str, String)],
    ) -> std::process::Output {
        let executable = std::env::current_exe().unwrap();
        let mut command = new_command(executable);
        command
            .args([
                "--ignored",
                "--exact",
                "runtime::tests::latest_harness_version_child",
                "--nocapture",
            ])
            .env_clear()
            .env("DSH_DESKTOP_NPM_REGISTRIES", registries)
            .env("DSH_DESKTOP_TEST_SYSTEM_PROXY", "1")
            .envs(extra.iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().unwrap()
    }

    #[test]
    #[ignore]
    fn latest_harness_version_child() {
        match latest_harness_version(&DeploymentController::default()) {
            Ok(version) => println!("VERSION={version}"),
            Err(error) => {
                println!("ERROR_CODE={}", error.code);
                println!("ERROR_DETAIL={}", error.safe_detail.unwrap_or_default());
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn latest_version_succeeds_when_at_least_one_registry_answers() {
        let (good, good_server) = serve_responses(vec![harness_packument("0.1.0-rc.7")]);
        let bad = closed_loopback_registry();
        let output = spawn_version_query_child(&format!("{bad},{good}"), &[]);
        good_server.join().unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("VERSION=0.1.0-rc.7"), "{stdout}");
    }

    #[test]
    fn failed_version_query_lists_each_registry_once_and_leaks_no_credentials() {
        let bad_a = closed_loopback_registry();
        let bad_b = closed_loopback_registry();
        let output = spawn_version_query_child(
            &format!("{bad_a},{bad_b}"),
            &[
                ("HTTP_PROXY", "http://user:topsecret@127.0.0.1:1".to_owned()),
                (
                    "HTTPS_PROXY",
                    "http://user:topsecret@127.0.0.1:1".to_owned(),
                ),
            ],
        );
        assert!(!output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("ERROR_CODE=versionQueryConnect"),
            "{stdout}"
        );
        for registry in [&bad_a, &bad_b] {
            let address = registry.trim_start_matches("http://");
            assert_eq!(
                stdout.matches(address).count(),
                1,
                "registry {address} must appear exactly once: {stdout}"
            );
        }
        assert!(!stdout.contains("topsecret"), "{stdout}");
        assert!(!stdout.contains("user@"), "{stdout}");
    }

    #[test]
    #[ignore]
    fn test_proxy_connection_child() {
        let settings: ProxySettings =
            serde_json::from_str(&std::env::var("DSH_PROXY_TEST_SETTINGS").unwrap()).unwrap();
        match test_proxy_connection(&settings) {
            Ok(report) => println!("REPORT={}", serde_json::to_string(&report).unwrap()),
            Err(error) => {
                println!("ERROR_CODE={}", error.code);
                println!("ERROR_DETAIL={}", error.safe_detail.unwrap_or_default());
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn proxy_connection_test_reports_each_registry_outcome() {
        let (good, good_server) = serve_responses(vec![harness_packument("0.1.0-rc.7")]);
        let bad = closed_loopback_registry();
        let executable = std::env::current_exe().unwrap();
        let mut command = new_command(executable);
        command
            .args([
                "--ignored",
                "--exact",
                "runtime::tests::test_proxy_connection_child",
                "--nocapture",
            ])
            .env_clear()
            .env("DSH_DESKTOP_NPM_REGISTRIES", format!("{bad},{good}"))
            .env(
                "DSH_PROXY_TEST_SETTINGS",
                serde_json::to_string(&ProxySettings {
                    mode: crate::model::ProxyMode::Direct,
                    ..ProxySettings::default()
                })
                .unwrap(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = command.output().unwrap();
        good_server.join().unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let report_line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("REPORT="))
            .expect("report line");
        let report: crate::model::ProxyTestReport =
            serde_json::from_str(report_line).expect("report json");
        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].version, "0.1.0-rc.7");
        assert_eq!(report.sources[0].source, good);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].source, bad);
        assert_eq!(report.failures[0].kind, NetworkErrorKind::Connect);
        // The per-registry detail must not repeat the registry address.
        assert!(
            !report.failures[0]
                .detail
                .contains(bad.trim_start_matches("http://")),
            "detail: {}",
            report.failures[0].detail
        );
    }

    #[test]
    fn proxy_connection_test_rejects_invalid_settings() {
        let invalid = ProxySettings {
            mode: crate::model::ProxyMode::Manual,
            url: "http://user:pw@127.0.0.1:8080".into(),
            bypass: String::new(),
        };
        let error = test_proxy_connection(&invalid).expect_err("userinfo must be rejected");
        assert_eq!(error.code, "proxyUrlInvalid");
    }

    #[test]
    fn version_query_failure_prefers_actionable_proxy_causes() {
        assert_eq!(
            primary_version_query_failure(&[
                NetworkErrorKind::Connect,
                NetworkErrorKind::ProxyAuth,
                NetworkErrorKind::Timeout,
            ]),
            Some(NetworkErrorKind::ProxyAuth)
        );
        assert_eq!(primary_version_query_failure(&[]), None);
    }
}
