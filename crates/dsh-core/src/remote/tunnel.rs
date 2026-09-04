//! Managed cloudflared quick-tunnel for public remote access.
//!
//! The binary is downloaded once from the official Cloudflare GitHub release,
//! validated against the SHA-256 pinned here (per the repository's download
//! policy), and then reused. The quick tunnel needs no account and yields a
//! random `*.trycloudflare.com` URL that changes every run — a natural
//! rotation: an old link dies with its process.
//!
//! The tunnel points at the launcher's own loopback proxy port, which is
//! stable across Harness restarts, so restarting or updating the Harness
//! service never tears down an open tunnel.
//!
//! Lifecycle: on Windows the child joins the shared kill-on-close job guard
//! (`runtime::WindowsProcessGuard`), so even a crashing launcher cannot
//! orphan a live tunnel. Other platforms rely on `stop()`/`Drop`, which run
//! on every controlled shutdown path.

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use regex::Regex;
use sha2::{Digest, Sha256};

#[cfg(windows)]
use crate::runtime::WindowsProcessGuard;
use crate::{
    AppError, AppResult, child_process::new_command, network, paths::ApplicationPaths,
    paths::atomic_write,
};

/// Pinned cloudflared release. Bumping requires refreshing every hash below.
const CLOUDFLARED_VERSION: &str = "2026.8.2";
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
/// Per-source ceiling: a slow or stalled source gives up quickly so the next
/// mirror can take over instead of blocking the toggle for minutes.
const SOURCE_TIMEOUT: Duration = Duration::from_secs(90);
const URL_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const URL_PENDING: u8 = 0;
const URL_REPORTED: u8 = 1;
const URL_TIMED_OUT: u8 = 2;
/// A quick off→on toggle can leave the stale bootstrap downloading while a
/// new generation starts. Serialize admission and installation so the new
/// worker reuses the first verified result instead of downloading and
/// publishing the same binary concurrently.
static BINARY_INSTALL_LOCK: Mutex<()> = Mutex::new(());

struct Artifact {
    os: &'static str,
    arch: &'static str,
    file: &'static str,
    sha256: &'static str,
    gzipped_tar: bool,
}

const ARTIFACTS: &[Artifact] = &[
    Artifact {
        os: "macos",
        arch: "x86_64",
        file: "cloudflared-darwin-amd64.tgz",
        sha256: "f1727723c586500e2092368ae21871b3df7ddfd2cb097f22d81bee4a9c458bb4",
        gzipped_tar: true,
    },
    Artifact {
        os: "macos",
        arch: "aarch64",
        file: "cloudflared-darwin-arm64.tgz",
        sha256: "9042c2c5d8b2de78e60f313d5fb31b6c5c1cebde787a3caf1f2c9588084ac442",
        gzipped_tar: true,
    },
    Artifact {
        os: "linux",
        arch: "x86_64",
        file: "cloudflared-linux-amd64",
        sha256: "fcfb02b575a52ca1af2e3267af4e1517bcdeb30ac48c834c69abaed3c0576ad2",
        gzipped_tar: false,
    },
    Artifact {
        os: "linux",
        arch: "aarch64",
        file: "cloudflared-linux-arm64",
        sha256: "7747d94570fb390cf47dcb4f9555c193c6355cda9793f0d878d9049e5d6a7790",
        gzipped_tar: false,
    },
    Artifact {
        os: "windows",
        arch: "x86_64",
        file: "cloudflared-windows-amd64.exe",
        sha256: "c29eee2b121f5436a642eed69fd9767da7e7b8c510fa50aaa130337f931357b5",
        gzipped_tar: false,
    },
];

fn artifact_for(os: &str, arch: &str) -> Option<&'static Artifact> {
    ARTIFACTS
        .iter()
        .find(|artifact| artifact.os == os && artifact.arch == arch)
}

/// Download sources, most canonical first. The mirrors are third-party
/// GitHub release proxies; falling back to them is safe because the payload
/// is admitted only when it matches the pinned SHA-256, so a malicious or
/// stale mirror cannot inject anything.
fn artifact_urls(artifact: &Artifact) -> Vec<String> {
    let release = format!(
        "github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/{}",
        artifact.file
    );
    vec![
        format!("https://{release}"),
        format!("https://gh-proxy.com/https://{release}"),
        format!("https://ghproxy.net/https://{release}"),
        format!("https://gh.ddlc.top/https://{release}"),
    ]
}

/// Ensures the pinned cloudflared binary exists and returns its path.
/// Downloads and hash-verifies on first use. After extracting an archive, the
/// verified binary's own hash is recorded in the version marker. A previously
/// installed binary is reused only when its recomputed hash still matches that
/// recorded value; a corrupted or replaced binary is never executed and falls
/// back to a fresh download instead.
pub fn ensure_binary(paths: &ApplicationPaths) -> AppResult<PathBuf> {
    let artifact = artifact_for(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| AppError::new("remoteTunnelUnsupported"))?;
    ensure_binary_with(paths, artifact, download)
}

fn ensure_binary_with(
    paths: &ApplicationPaths,
    artifact: &Artifact,
    fetch: impl FnOnce(&Artifact) -> AppResult<Vec<u8>>,
) -> AppResult<PathBuf> {
    let _install_guard = BINARY_INSTALL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let marker = paths
        .remote_dir
        .join(format!(".cloudflared-{CLOUDFLARED_VERSION}"));
    if let Ok(expected_binary_hash) = fs::read_to_string(&marker)
        && let Some(binary) = reusable_binary(paths, expected_binary_hash.trim())
    {
        return Ok(binary);
    }
    let bytes = fetch(artifact)?;
    verify(artifact, &bytes)?;
    let installed_hash = install(paths, artifact, &bytes)?;
    atomic_write(&marker, format!("{installed_hash}\n").as_bytes())?;
    Ok(paths.cloudflared_bin.clone())
}

/// Returns the installed binary only when it still hashes to the value
/// recorded immediately after a pinned artifact was verified and installed.
/// Archive artifacts and their extracted binaries intentionally have
/// different hashes, so comparing the installed file to the archive pin would
/// force a download on every launch on macOS.
fn reusable_binary(paths: &ApplicationPaths, expected_sha256: &str) -> Option<PathBuf> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let bytes = fs::read(&paths.cloudflared_bin).ok()?;
    if hex::encode(Sha256::digest(&bytes)) != expected_sha256.to_ascii_lowercase() {
        return None;
    }
    Some(paths.cloudflared_bin.clone())
}

/// Tries every download source in turn and returns the first payload that
/// arrives intact. Each source gets its own deadline so a stalled source
/// (common for GitHub release assets on restricted networks) yields to the
/// next mirror instead of blocking the toggle for minutes.
fn download(artifact: &Artifact) -> AppResult<Vec<u8>> {
    // The builder applies the launcher's unified proxy policy.
    let client = network::active_blocking_client("dsh-desktop-remote")?;
    let mut last_error: Option<AppError> = None;
    for url in artifact_urls(artifact) {
        match download_from(&client, &url) {
            Ok(bytes) if bytes.len() as u64 <= MAX_DOWNLOAD_BYTES => return Ok(bytes),
            Ok(_) => last_error = Some(AppError::new("remoteTunnelDownloadFailed")),
            Err(error) => {
                log::info!("cloudflared download source failed: {error}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| AppError::new("remoteTunnelDownloadFailed")))
}

fn download_from(client: &reqwest::blocking::Client, url: &str) -> AppResult<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(SOURCE_TIMEOUT)
        .send()
        .map_err(|error| classify_download(&error))?;
    let response = response.error_for_status().map_err(|error| {
        AppError::new("remoteTunnelDownloadFailed")
            .detail(network::sanitize_detail(&error.to_string()))
    })?;
    let mut bytes = Vec::with_capacity(32 * 1024 * 1024);
    let mut reader = response.take(MAX_DOWNLOAD_BYTES + 1);
    let deadline = Instant::now() + SOURCE_TIMEOUT;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if Instant::now() > deadline {
            return Err(AppError::new("remoteTunnelDownloadTimedOut"));
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) => return Err(AppError::io("remoteTunnelDownloadFailed", &error)),
        }
    }
    Ok(bytes)
}

fn classify_download(error: &reqwest::Error) -> AppError {
    let classified = network::classify_reqwest(error);
    let code = match classified.kind {
        crate::NetworkErrorKind::Timeout => "remoteTunnelDownloadTimedOut",
        _ => "remoteTunnelDownloadFailed",
    };
    AppError::new(code).detail(classified.detail)
}

fn verify(artifact: &Artifact, bytes: &[u8]) -> AppResult<()> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != artifact.sha256 {
        return Err(AppError::new("checksumMismatch"));
    }
    Ok(())
}

fn install(paths: &ApplicationPaths, artifact: &Artifact, bytes: &[u8]) -> AppResult<String> {
    let binary = if artifact.gzipped_tar {
        extract_tgz(bytes)?
    } else {
        bytes.to_vec()
    };
    let installed_hash = hex::encode(Sha256::digest(&binary));
    atomic_write(&paths.cloudflared_bin, &binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paths.cloudflared_bin, fs::Permissions::from_mode(0o755))?;
    }
    Ok(installed_hash)
}

/// Extracts the single `cloudflared` entry from the macOS tarball. Entry
/// names are validated so an archive can never write outside the target.
fn extract_tgz(bytes: &[u8]) -> AppResult<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| AppError::io("archiveInvalid", &error))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| AppError::io("archiveInvalid", &error))?;
        let path = entry
            .path()
            .map_err(|error| AppError::io("archiveInvalid", &error))?
            .into_owned();
        let safe = path.components().count() == 1
            && path.file_name().is_some_and(|name| name == "cloudflared");
        if !safe {
            continue;
        }
        let mut binary = Vec::new();
        entry
            .read_to_end(&mut binary)
            .map_err(|error| AppError::io("archiveInvalid", &error))?;
        return Ok(binary);
    }
    Err(AppError::new("archiveInvalid").detail("cloudflared entry missing"))
}

/// Outcome callbacks for the tunnel worker. Both fire at most once.
/// `on_exit` always carries an error: deliberate stops report nothing, so
/// any reported exit — even a clean status 0 — is unexpected and failed.
pub(crate) trait TunnelEvents: Send + Sync {
    fn on_url(&self, url: String);
    fn on_exit(&self, error: Option<AppError>);
}

/// A running cloudflared child. `stop()` (also run on drop) kills it.
pub(crate) struct TunnelProcess {
    child: Mutex<Option<Child>>,
    watchers: Mutex<Vec<JoinHandle<()>>>,
    stopped: AtomicBool,
    /// Coordinates the log and deadline watchers. Only one can transition
    /// Pending to Reported/TimedOut, so a URL arriving at the deadline can
    /// never publish Running after a timeout already published Failed.
    url_state: AtomicU8,
    /// Set by the exit watcher before it reports, so a completion racing the
    /// report never resurrects a dead child into the service state.
    exited: AtomicBool,
    /// Only the first of the exit watcher and URL-deadline watcher reports an
    /// error to the service.
    exit_reported: AtomicBool,
    /// Windows kill-on-close job: the OS kills cloudflared when the launcher
    /// dies, even on a crash. Kept alive for the child's lifetime.
    #[cfg(windows)]
    _guard: Option<WindowsProcessGuard>,
}

impl TunnelProcess {
    /// Spawns `cloudflared tunnel --url http://127.0.0.1:<port>` without a
    /// shell, scans its log for the assigned public URL, and reports both the
    /// URL and an unexpected exit through the events sink.
    pub(crate) fn spawn(
        binary: &Path,
        local_port: u16,
        events: Arc<dyn TunnelEvents>,
    ) -> AppResult<Arc<Self>> {
        let mut command = new_command(binary);
        command
            .args([
                "tunnel",
                "--no-autoupdate",
                "--url",
                &format!("http://127.0.0.1:{local_port}"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_clear();
        network::apply_to_command(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| AppError::io("remoteTunnelFailed", &error))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("remoteTunnelFailed"))?;
        // Parent-lifecycle cleanup: on Windows the child must join a
        // kill-on-close job so a crashing launcher never orphans a live
        // tunnel. Do not publish an unguarded child if both the Job and the
        // existing snapshot fallback are unavailable.
        #[cfg(windows)]
        let guard = match WindowsProcessGuard::attach(&child) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let process = Arc::new(Self {
            child: Mutex::new(Some(child)),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: guard,
        });
        let reader = {
            let process = Arc::clone(&process);
            let events = Arc::clone(&events);
            thread::Builder::new()
                .name("remote-tunnel-log".into())
                .spawn(move || scan_tunnel_log(stderr, events, process))
                .map_err(|error| AppError::io("remoteTunnelFailed", &error))?
        };
        process
            .watchers
            .lock()
            .expect("watchers poisoned")
            .push(reader);
        let waiter_result = {
            let process = Arc::clone(&process);
            let events = Arc::clone(&events);
            thread::Builder::new()
                .name("remote-tunnel-wait".into())
                .spawn(move || process.wait_and_report(events))
        };
        let waiter = match waiter_result {
            Ok(waiter) => waiter,
            Err(error) => {
                process.stop();
                return Err(AppError::io("remoteTunnelFailed", &error));
            }
        };
        process
            .watchers
            .lock()
            .expect("watchers poisoned")
            .push(waiter);
        let deadline_result = {
            let process = Arc::clone(&process);
            thread::Builder::new()
                .name("remote-tunnel-url-deadline".into())
                .spawn(move || wait_for_url_or_timeout(process, events, URL_WAIT_TIMEOUT))
        };
        let deadline = match deadline_result {
            Ok(deadline) => deadline,
            Err(error) => {
                process.stop();
                return Err(AppError::io("remoteTunnelFailed", &error));
            }
        };
        process
            .watchers
            .lock()
            .expect("watchers poisoned")
            .push(deadline);
        Ok(process)
    }

    fn wait_and_report(&self, events: Arc<dyn TunnelEvents>) {
        // Poll with try_wait instead of holding the child lock across a
        // blocking wait: stop() must always be able to take and kill the
        // child without deadlocking against this watcher.
        let status = loop {
            {
                let mut guard = self.child.lock().expect("tunnel child poisoned");
                match guard.as_mut() {
                    None => break None, // stop() took the child.
                    Some(child) => {
                        if let Ok(Some(status)) = child.try_wait() {
                            break Some(Ok(status));
                        }
                    }
                }
            }
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        };
        if self.stopped.load(Ordering::SeqCst) {
            return; // Deliberate stop is not a failure.
        }
        // Mark before reporting so a bootstrap completion racing this
        // callback can never store an already-dead process.
        self.exited.store(true, Ordering::SeqCst);
        let error = match status {
            // A clean exit is still unexpected for a tunnel that should
            // outlive the desktop session; the service maps it to Failed.
            Some(Ok(status)) => Some(
                AppError::new("remoteTunnelFailed").detail(format!("cloudflared exited: {status}")),
            ),
            Some(Err(error)) => Some(AppError::io("remoteTunnelFailed", &error)),
            None => Some(AppError::new("remoteTunnelFailed")),
        };
        // Wake the URL deadline and log watchers before publishing the exit.
        // Without this, a silent child that exits before yielding a URL can
        // keep the process alive until the full URL timeout expires.
        self.stopped.store(true, Ordering::SeqCst);
        self.report_exit(events, error);
    }

    fn report_exit(&self, events: Arc<dyn TunnelEvents>, error: Option<AppError>) {
        if self
            .exit_reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            events.on_exit(error);
        }
    }

    /// True once the exit watcher observed the child terminate.
    pub(crate) fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Kills the child without joining the watcher threads. Unlike `stop`,
    /// this is safe to call from inside a watcher thread: joining the
    /// calling thread would abort the process (panic = abort) on Unix or
    /// deadlock on Windows.
    fn terminate_child(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().expect("tunnel child poisoned").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[cfg(test)]
    /// A process that has already exited, for service-side race tests.
    pub(crate) fn exited_stub() -> Arc<Self> {
        Arc::new(Self {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(true),
            exit_reported: AtomicBool::new(true),
            #[cfg(windows)]
            _guard: None,
        })
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().expect("tunnel child poisoned").take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let current_thread = thread::current().id();
        for watcher in self.watchers.lock().expect("watchers poisoned").drain(..) {
            // A watcher may own the final Arc after an early spawn/exit race,
            // causing Drop to run on that same watcher. Dropping its handle
            // detaches it; joining itself would panic or deadlock.
            if watcher.thread().id() != current_thread {
                let _ = watcher.join();
            }
        }
    }
}

impl Drop for TunnelProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn scan_tunnel_log(stderr: impl Read, events: Arc<dyn TunnelEvents>, process: Arc<TunnelProcess>) {
    let pattern =
        Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com").expect("tunnel url pattern");
    for line in BufReader::new(stderr).lines() {
        if process.stopped.load(Ordering::SeqCst) {
            return;
        }
        let Ok(line) = line else { return };
        if let Some(found) = pattern.find(&line)
            && process
                .url_state
                .compare_exchange(
                    URL_PENDING,
                    URL_REPORTED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
        {
            events.on_url(found.as_str().to_owned());
        }
        // Keep draining stderr for the entire child lifetime. Closing the
        // pipe as soon as cloudflared prints its public URL makes its later
        // log writes receive SIGPIPE; on macOS that killed every restored
        // public tunnel and exercised the launcher's watcher cleanup crash
        // path on every subsequent start.
    }
}

/// Enforces URL_WAIT_TIMEOUT independently of stderr activity. cloudflared
/// can remain alive without emitting another complete line, so checking a
/// deadline only inside the line-reader loop can leave the UI in Starting
/// forever.
fn wait_for_url_or_timeout(
    process: Arc<TunnelProcess>,
    events: Arc<dyn TunnelEvents>,
    url_wait: Duration,
) {
    let deadline = Instant::now() + url_wait;
    loop {
        if process.stopped.load(Ordering::SeqCst)
            || process.url_state.load(Ordering::SeqCst) != URL_PENDING
        {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            if process
                .url_state
                .compare_exchange(
                    URL_PENDING,
                    URL_TIMED_OUT,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
            {
                return;
            }
            // Never call `stop()` here: it joins the watcher threads, and
            // this *is* one of them — joining self aborts the process
            // (panic = abort) on Unix and deadlocks on Windows. Kill the
            // child, then report the timeout so the scope lands in Failed
            // and stays retryable.
            process.exited.store(true, Ordering::SeqCst);
            process.terminate_child();
            process.report_exit(events, Some(AppError::new("remoteTunnelTimeout")));
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_platform_has_a_pinned_artifact() {
        for (os, arch) in [
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
        ] {
            let artifact = artifact_for(os, arch).unwrap_or_else(|| panic!("{os}/{arch}"));
            assert_eq!(artifact.sha256.len(), 64);
            assert!(
                artifact_urls(artifact)
                    .iter()
                    .all(|url| url.contains(CLOUDFLARED_VERSION))
            );
            assert!(
                artifact_urls(artifact).len() > 1,
                "mirror fallbacks required"
            );
        }
        assert!(artifact_for("freebsd", "x86_64").is_none());
    }

    #[test]
    fn checksum_verification_rejects_tampering() {
        let artifact = artifact_for("linux", "x86_64").unwrap();
        assert!(verify(artifact, b"not the binary").is_err());
        let error = verify(artifact, b"not the binary").unwrap_err();
        assert_eq!(error.code, "checksumMismatch");
    }

    #[test]
    fn tgz_extraction_rejects_traversal_and_finds_the_binary() {
        use flate2::{Compression, write::GzEncoder};
        let build = |entries: &[(&str, &[u8])]| {
            let encoder = GzEncoder::new(Vec::new(), Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            for (name, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header.clone(), name, *body)
                    .unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap()
        };
        let good = build(&[
            ("subdir/cloudflared", b"bin"),
            ("cloudflared", b"real-binary"),
        ]);
        // Only a bare top-level `cloudflared` entry is accepted; the nested
        // decoy must be skipped even though it appears first.
        assert_eq!(extract_tgz(&good).unwrap(), b"real-binary");
        let evil = build(&[("dir/cloudflared", b"x"), ("other.txt", b"x")]);
        assert!(extract_tgz(&evil).is_err());
    }

    #[test]
    fn install_marks_binary_executable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        let artifact = artifact_for("linux", "x86_64").unwrap();
        let installed_hash = install(&paths, artifact, b"fake-binary").unwrap();
        assert_eq!(installed_hash, hex::encode(Sha256::digest(b"fake-binary")));
        assert_eq!(fs::read(&paths.cloudflared_bin).unwrap(), b"fake-binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&paths.cloudflared_bin)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn concurrent_bootstraps_share_one_binary_download() {
        static FAKE_ARTIFACT: Artifact = Artifact {
            os: "test",
            arch: "test",
            file: "cloudflared-test",
            sha256: "b7491ad78a468e4edcd2ecae95a54ebd8cfeb7dfd758ba92f1eff843875113ae",
            gzipped_tar: false,
        };
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        let starts = Arc::new(std::sync::Barrier::new(3));
        let downloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let paths = paths.clone();
            let starts = Arc::clone(&starts);
            let downloads = Arc::clone(&downloads);
            workers.push(thread::spawn(move || {
                starts.wait();
                ensure_binary_with(&paths, &FAKE_ARTIFACT, |_| {
                    downloads.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(50));
                    Ok(b"fake-cloudflared\n".to_vec())
                })
                .unwrap()
            }));
        }

        starts.wait();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), paths.cloudflared_bin);
        }
        assert_eq!(downloads.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(&paths.cloudflared_bin).unwrap(),
            b"fake-cloudflared\n"
        );
    }

    #[test]
    fn tunnel_log_scan_reports_the_assigned_url() {
        struct Sink {
            url: Mutex<Option<String>>,
        }
        impl TunnelEvents for Sink {
            fn on_url(&self, url: String) {
                *self.url.lock().unwrap() = Some(url);
            }
            fn on_exit(&self, _error: Option<AppError>) {}
        }
        let sink = Arc::new(Sink {
            url: Mutex::new(None),
        });
        let log = b"2026-08-01 INF +--------------------------------------------------------------------------------------------+\n2026-08-01 INF |  Your quick Tunnel has been created! Visit it at: https://words-go-here.trycloudflare.com  |\n";
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });
        scan_tunnel_log(&log[..], sink.clone(), Arc::clone(&process));
        assert_eq!(
            sink.url.lock().unwrap().as_deref(),
            Some("https://words-go-here.trycloudflare.com")
        );
        assert_eq!(process.url_state.load(Ordering::SeqCst), URL_REPORTED);
    }

    /// cloudflared keeps writing diagnostics after publishing the quick-
    /// tunnel URL. The parent must keep stderr open and drain it until EOF;
    /// returning after the URL closes the pipe and kills cloudflared with
    /// SIGPIPE on macOS.
    #[test]
    fn tunnel_log_scan_keeps_draining_after_the_assigned_url() {
        struct Sink;
        impl TunnelEvents for Sink {
            fn on_url(&self, _url: String) {}
            fn on_exit(&self, _error: Option<AppError>) {}
        }

        struct BytewiseReader {
            input: std::io::Cursor<Vec<u8>>,
            consumed: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Read for BytewiseReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let maximum = output.len().min(1);
                let length = self.input.read(&mut output[..maximum])?;
                self.consumed.fetch_add(length, Ordering::SeqCst);
                Ok(length)
            }
        }

        let log = b"https://words-go-here.trycloudflare.com\npost-url diagnostic\n".to_vec();
        let consumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = BytewiseReader {
            input: std::io::Cursor::new(log.clone()),
            consumed: Arc::clone(&consumed),
        };
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });

        scan_tunnel_log(reader, Arc::new(Sink), process);

        assert_eq!(consumed.load(Ordering::SeqCst), log.len());
    }

    #[cfg(unix)]
    #[test]
    fn live_tunnel_survives_logging_after_the_assigned_url() {
        use std::os::unix::fs::PermissionsExt;

        struct StopOnDrop(Arc<TunnelProcess>);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.stop();
            }
        }

        struct Sink {
            url: Mutex<Option<String>>,
            exits: std::sync::atomic::AtomicUsize,
        }
        impl TunnelEvents for Sink {
            fn on_url(&self, url: String) {
                *self.url.lock().unwrap() = Some(url);
            }
            fn on_exit(&self, _error: Option<AppError>) {
                self.exits.fetch_add(1, Ordering::SeqCst);
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("fake-cloudflared");
        fs::write(
            &binary,
            "#!/bin/sh\nset -e\nprintf '%s\\n' 'https://still-running.trycloudflare.com' >&2\nwhile [ ! -f \"$0.continue\" ]; do /bin/sleep 0.05; done\ni=0\nwhile [ \"$i\" -lt 4096 ]; do printf '%s\\n' 'post-url diagnostic' >&2; i=$((i + 1)); done\n: > \"$0.logged\"\nwhile :; do /bin/sleep 0.05; done\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let sink = Arc::new(Sink {
            url: Mutex::new(None),
            exits: std::sync::atomic::AtomicUsize::new(0),
        });
        let events = Arc::clone(&sink) as Arc<dyn TunnelEvents>;
        let process = TunnelProcess::spawn(&binary, 1, events).unwrap();
        let _cleanup = StopOnDrop(Arc::clone(&process));

        let deadline = Instant::now() + Duration::from_secs(10);
        while sink.url.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            sink.url.lock().unwrap().as_deref(),
            Some("https://still-running.trycloudflare.com")
        );
        // Release the writer only after the URL callback. More than a pipe
        // buffer of diagnostics must drain before the child acknowledges it;
        // its lifetime must not depend on scheduler speed during the suite.
        fs::write(binary.with_extension("continue"), b"").unwrap();
        let logged = binary.with_extension("logged");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !logged.exists() && !process.has_exited() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(logged.exists(), "post-URL diagnostics did not drain");
        assert_eq!(sink.exits.load(Ordering::SeqCst), 0);
        assert!(!process.has_exited());

        process.stop();
        assert_eq!(sink.exits.load(Ordering::SeqCst), 0);
    }

    /// Regression for both timeout failures: the deadline used to be checked
    /// only after reading a log line, and its timeout branch called `stop()`,
    /// which joined the calling watcher. A completely silent child therefore
    /// waited forever; a logging child could abort on Unix or deadlock on
    /// Windows. The independent watcher must report and exit cleanly.
    #[test]
    fn silent_url_timeout_reports_without_joining_itself() {
        struct Sink {
            exit: Mutex<Option<Option<AppError>>>,
        }
        impl TunnelEvents for Sink {
            fn on_url(&self, _url: String) {
                panic!("no URL may be reported on the timeout path");
            }
            fn on_exit(&self, error: Option<AppError>) {
                *self.exit.lock().unwrap() = Some(error);
            }
        }

        let sink = Arc::new(Sink {
            exit: Mutex::new(None),
        });
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });
        // Run the deadline watcher as a registered watcher, exactly like
        // production. It receives no log input at all: the timeout must not
        // depend on cloudflared emitting another line.
        let deadline = {
            let events = Arc::clone(&sink) as Arc<dyn TunnelEvents>;
            let process = Arc::clone(&process);
            thread::spawn(move || {
                wait_for_url_or_timeout(process, events, Duration::from_millis(50));
            })
        };
        process
            .watchers
            .lock()
            .expect("watchers poisoned")
            .push(deadline);
        // Wait for the timeout report (the watcher finishes on its own).
        let wait_start = Instant::now();
        loop {
            if let Some(exit) = sink.exit.lock().unwrap().as_ref() {
                assert_eq!(
                    exit.as_ref().expect("timeout is an error").code,
                    "remoteTunnelTimeout"
                );
                break;
            }
            assert!(
                wait_start.elapsed() < Duration::from_secs(5),
                "timeout path must report remoteTunnelTimeout promptly"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(process.stopped.load(Ordering::SeqCst));
        assert!(process.exited.load(Ordering::SeqCst));
        assert_eq!(process.url_state.load(Ordering::SeqCst), URL_TIMED_OUT);
        // A later stop() (service teardown) joins the finished watcher
        // cleanly instead of hanging.
        process.stop();
    }

    #[test]
    fn reported_url_prevents_a_later_timeout_failure() {
        struct Sink {
            url: Mutex<Option<String>>,
            exits: std::sync::atomic::AtomicUsize,
        }
        impl TunnelEvents for Sink {
            fn on_url(&self, url: String) {
                *self.url.lock().unwrap() = Some(url);
            }
            fn on_exit(&self, _error: Option<AppError>) {
                self.exits.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(Sink {
            url: Mutex::new(None),
            exits: std::sync::atomic::AtomicUsize::new(0),
        });
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });
        let events = Arc::clone(&sink) as Arc<dyn TunnelEvents>;
        scan_tunnel_log(
            &b"https://race-winner.trycloudflare.com\n"[..],
            Arc::clone(&events),
            Arc::clone(&process),
        );
        wait_for_url_or_timeout(Arc::clone(&process), events, Duration::ZERO);

        assert_eq!(
            sink.url.lock().unwrap().as_deref(),
            Some("https://race-winner.trycloudflare.com")
        );
        assert_eq!(sink.exits.load(Ordering::SeqCst), 0);
        assert_eq!(process.url_state.load(Ordering::SeqCst), URL_REPORTED);
        assert!(!process.stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn exit_is_reported_at_most_once() {
        struct Sink(std::sync::atomic::AtomicUsize);
        impl TunnelEvents for Sink {
            fn on_url(&self, _url: String) {}
            fn on_exit(&self, _error: Option<AppError>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(Sink(std::sync::atomic::AtomicUsize::new(0)));
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });
        let events = Arc::clone(&sink) as Arc<dyn TunnelEvents>;
        process.report_exit(Arc::clone(&events), Some(AppError::new("first")));
        process.report_exit(events, Some(AppError::new("second")));
        assert_eq!(sink.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_process_reference_can_drop_inside_its_own_watcher() {
        use std::sync::mpsc;

        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let process = Arc::new(TunnelProcess {
            child: Mutex::new(None),
            watchers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            url_state: AtomicU8::new(URL_PENDING),
            exited: AtomicBool::new(false),
            exit_reported: AtomicBool::new(false),
            #[cfg(windows)]
            _guard: None,
        });
        let watcher_process = Arc::clone(&process);
        let watcher = thread::spawn(move || {
            release_rx.recv().unwrap();
            drop(watcher_process);
            done_tx.send(()).unwrap();
        });
        process
            .watchers
            .lock()
            .expect("watchers poisoned")
            .push(watcher);

        // The watcher now owns the only process reference. Drop therefore
        // runs on that watcher and must detach, rather than join, itself.
        drop(process);
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("self-owned process drop must complete");
    }

    #[test]
    fn archived_binary_is_reused_only_when_its_installed_hash_still_matches() {
        use flate2::{Compression, write::GzEncoder};

        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        let payload = b"fake-cloudflared";
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "cloudflared", &payload[..])
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let archive_hash: &'static str =
            Box::leak(hex::encode(Sha256::digest(&archive)).into_boxed_str());
        let artifact = Artifact {
            os: "test",
            arch: "test",
            file: "cloudflared-test.tgz",
            sha256: archive_hash,
            gzipped_tar: true,
        };

        verify(&artifact, &archive).unwrap();
        let installed_hash = install(&paths, &artifact, &archive).unwrap();
        assert_ne!(
            installed_hash, artifact.sha256,
            "archive and extracted binary deliberately have different hashes"
        );
        assert_eq!(
            reusable_binary(&paths, &installed_hash),
            Some(paths.cloudflared_bin.clone())
        );

        // Tampered content must never be executed; reuse is refused so the
        // caller falls back to a fresh verified download.
        fs::write(&paths.cloudflared_bin, b"tampered").unwrap();
        assert_eq!(reusable_binary(&paths, &installed_hash), None);

        fs::remove_file(&paths.cloudflared_bin).unwrap();
        assert_eq!(reusable_binary(&paths, &installed_hash), None);
        assert_eq!(reusable_binary(&paths, "2026.8.2"), None);
    }
}
