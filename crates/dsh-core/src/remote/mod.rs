//! Remote access: exposes the loopback-only Harness web UI to the operator's
//! phone through authenticated reverse proxies (LAN + cloudflared tunnel).
//!
//! Design:
//! - One proxy listener per scope. LAN binds the wildcard address; the public
//!   listener binds loopback and only ever serves the cloudflared child.
//! - The proxies and the tunnel are owned by the launcher process and are
//!   independent of the Harness service lifecycle: the upstream address is
//!   resolved per connection, so a Harness restart or update never interrupts
//!   remote sessions, and an open tunnel survives it too.
//! - Passwords persist under the desktop-owned `remote/` directory (never
//!   DSH_HOME); sessions are in-memory and die with the launcher.
//! - Enabling public access requires an explicit disclaimer acknowledgement
//!   from the UI; the server enforces it so it cannot be bypassed.

mod auth;
mod lan;
mod server;
mod tunnel;

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

#[cfg(test)]
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::{
    AppError, AppResult,
    model::{
        RemoteLanSnapshot, RemotePublicSnapshot, RemoteScope, RemoteSnapshot, RemoteTunnelState,
    },
    paths::{ApplicationPaths, atomic_write},
};
use auth::{generate_password, is_valid_password};
use server::{AuthState, ProxyServer};
use tunnel::{TunnelEvents, TunnelProcess};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteSettings {
    /// Master switch. Defaults off: remote exposure is always opt-in.
    #[serde(default)]
    master: bool,
    #[serde(default = "default_enabled")]
    lan_enabled: bool,
    #[serde(default)]
    public_enabled: bool,
    #[serde(default = "generate_password")]
    lan_password: String,
    #[serde(default = "generate_password")]
    public_password: String,
}

fn default_enabled() -> bool {
    true
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            master: false,
            lan_enabled: true,
            public_enabled: false,
            lan_password: generate_password(),
            public_password: generate_password(),
        }
    }
}

impl RemoteSettings {
    fn load(paths: &ApplicationPaths) -> Self {
        let Ok(bytes) = std::fs::read(&paths.remote_settings_file) else {
            return Self::default();
        };
        let mut settings: Self = serde_json::from_slice(&bytes).unwrap_or_default();
        // Hand-edited or corrupted passwords must never weaken the door.
        if !is_valid_password(&settings.lan_password) {
            settings.lan_password = generate_password();
        }
        if !is_valid_password(&settings.public_password) {
            settings.public_password = generate_password();
        }
        settings
    }

    fn save(&self, paths: &ApplicationPaths) -> AppResult<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        atomic_write(&paths.remote_settings_file, &bytes)
    }
}

struct RemoteInner {
    settings: RemoteSettings,
    lan_server: Option<ProxyServer>,
    tunnel_listener: Option<ProxyServer>,
    tunnel: Option<Arc<TunnelProcess>>,
    public_state: RemoteTunnelState,
    public_url: Option<String>,
    public_error: Option<AppError>,
}

impl RemoteInner {
    fn new(settings: RemoteSettings) -> Self {
        Self {
            settings,
            lan_server: None,
            tunnel_listener: None,
            tunnel: None,
            public_state: RemoteTunnelState::Off,
            public_url: None,
            public_error: None,
        }
    }
}

/// Owns the proxy listeners, the tunnel child, and the persisted switches.
/// All state transitions emit through the change listener exactly once.
pub struct RemoteService {
    paths: ApplicationPaths,
    inner: std::sync::Mutex<RemoteInner>,
    /// Current Harness web authority (`127.0.0.1:port`), resolved per
    /// proxied connection so Harness restarts are transparent.
    upstream: Arc<RwLock<Option<String>>>,
    lan_auth: Arc<AuthState>,
    public_auth: Arc<AuthState>,
    listener: RwLock<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Bumped on every public-side transition; stale tunnel workers and
    /// late cloudflared callbacks discard their results against it.
    public_generation: Arc<AtomicU64>,
    /// Self-reference for tunnel worker callbacks, installed by `new`.
    self_weak: RwLock<std::sync::Weak<RemoteService>>,
    /// Test-only: forces `apply` to fail so setter error paths are testable
    /// without occupying real ports.
    #[cfg(test)]
    apply_fails: AtomicBool,
}

impl RemoteService {
    pub fn new(paths: ApplicationPaths) -> AppResult<Arc<Self>> {
        let settings = RemoteSettings::load(&paths);
        let service = Arc::new(Self {
            lan_auth: AuthState::new(RemoteScope::Lan, settings.lan_password.clone()),
            public_auth: AuthState::new(RemoteScope::Public, settings.public_password.clone()),
            inner: std::sync::Mutex::new(RemoteInner::new(settings)),
            upstream: Arc::new(RwLock::new(None)),
            listener: RwLock::new(None),
            public_generation: Arc::new(AtomicU64::new(0)),
            self_weak: RwLock::new(std::sync::Weak::new()),
            #[cfg(test)]
            apply_fails: AtomicBool::new(false),
            paths,
        });
        *service.self_weak.write().expect("self weak poisoned") = Arc::downgrade(&service);
        // Restore previously enabled switches; failures degrade to logged
        // errors instead of breaking launcher startup.
        if let Err(error) = service.apply(false) {
            log::warn!("remote access could not be restored: {error}");
        }
        Ok(service)
    }

    pub fn set_change_listener(&self, listener: Box<dyn Fn() + Send + Sync>) {
        *self.listener.write().expect("listener poisoned") = Some(listener);
        // Tunnel restoration starts in `new`, before the application shell
        // can install its listener. Publish the latest snapshot immediately
        // so a URL/failure that completed in that window is not lost and the
        // UI cannot remain stuck on the earlier Starting snapshot.
        self.notify();
    }

    fn notify(&self) {
        if let Some(listener) = self.listener.read().expect("listener poisoned").as_ref() {
            listener();
        }
    }

    /// Updates the Harness web authority the proxies forward to. Called by
    /// the application shell whenever the published web URL changes.
    pub fn set_upstream(&self, web_url: Option<&str>) -> AppResult<()> {
        let authority = web_url.map(upstream_authority).transpose()?;
        let changed = {
            let mut upstream = self.upstream.write().expect("upstream poisoned");
            let changed = *upstream != authority;
            *upstream = authority;
            changed
        };
        if changed {
            self.notify();
        }
        Ok(())
    }

    pub fn set_master(&self, enabled: bool) -> AppResult<()> {
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            let mut settings = inner.settings.clone();
            settings.master = enabled;
            settings.save(&self.paths)?;
            inner.settings = settings;
        }
        let result = self.apply(false);
        // Notify even when applying failed: the switch is persisted and the
        // snapshot mirrors it, so the UI must not disagree with the state
        // the next launch will restore.
        self.notify();
        result
    }

    pub fn set_lan_enabled(&self, enabled: bool) -> AppResult<()> {
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            let mut settings = inner.settings.clone();
            settings.lan_enabled = enabled;
            settings.save(&self.paths)?;
            inner.settings = settings;
        }
        let result = self.apply(false);
        self.notify();
        result
    }

    /// Enables or disables public access. Enabling requires the disclaimer
    /// acknowledgement; the check lives here, server-side, so the UI cannot
    /// be tricked into skipping it.
    pub fn set_public_enabled(&self, enabled: bool, acknowledged: bool) -> AppResult<()> {
        if enabled && !acknowledged {
            return Err(AppError::new("remoteDisclaimerRequired"));
        }
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            let mut settings = inner.settings.clone();
            settings.public_enabled = enabled;
            settings.save(&self.paths)?;
            inner.settings = settings;
        }
        let result = self.apply(false);
        self.notify();
        result
    }

    /// Rotates a scope password and revokes every session of that scope.
    pub fn rotate_password(&self, scope: RemoteScope) -> AppResult<()> {
        self.apply_password(scope, generate_password())
    }

    /// Sets a user-chosen scope password. Exactly eight ASCII digits, same as
    /// generated ones; any change revokes every session of that scope.
    pub fn set_password(&self, scope: RemoteScope, password: String) -> AppResult<()> {
        if !is_valid_password(&password) {
            return Err(AppError::new("remotePasswordInvalid"));
        }
        self.apply_password(scope, password)
    }

    fn apply_password(&self, scope: RemoteScope, password: String) -> AppResult<()> {
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            let mut settings = inner.settings.clone();
            match scope {
                RemoteScope::Lan => settings.lan_password = password.clone(),
                RemoteScope::Public => settings.public_password = password.clone(),
            }
            settings.save(&self.paths)?;
            inner.settings = settings;
        }
        let auth = match scope {
            RemoteScope::Lan => &self.lan_auth,
            RemoteScope::Public => &self.public_auth,
        };
        *auth.password.write().expect("password poisoned") = password;
        // Revoking the sessions alone would leave open WebSockets proxied
        // forever; a password change signs out every connected device.
        auth.revoke_all_connections();
        self.notify();
        Ok(())
    }

    /// Re-runs tunnel bootstrap after a failure without forcing the user to
    /// toggle the switch off and on. Only meaningful while public access is
    /// enabled and the tunnel is down; a healthy or starting tunnel makes it
    /// a no-op.
    pub fn retry_public_tunnel(&self) -> AppResult<()> {
        {
            let inner = self.inner.lock().expect("remote poisoned");
            if !(inner.settings.master && inner.settings.public_enabled) {
                return Err(AppError::new("remoteUnavailable"));
            }
            if inner.public_state != RemoteTunnelState::Failed {
                return Ok(());
            }
        }
        self.apply(true)?;
        self.notify();
        Ok(())
    }

    /// Starts/stops listeners and the tunnel to match the persisted switches.
    fn apply(&self, retry_failed_tunnel: bool) -> AppResult<()> {
        #[cfg(test)]
        if self.apply_fails.load(Ordering::SeqCst) {
            return Err(AppError::new("remoteListenFailed"));
        }
        let mut bootstrap = None;
        let mut lan_to_stop = None;
        let mut tunnel_to_stop = None;
        let mut tunnel_listener_to_stop = None;
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            let want_lan = inner.settings.master && inner.settings.lan_enabled;
            let want_public = inner.settings.master && inner.settings.public_enabled;

            if want_lan && inner.lan_server.is_none() {
                inner.lan_server = Some(ProxyServer::bind(
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    Arc::clone(&self.lan_auth),
                    Arc::clone(&self.upstream),
                )?);
            } else if !want_lan {
                lan_to_stop = inner.lan_server.take();
            }

            if want_public && inner.tunnel_listener.is_none() {
                inner.tunnel_listener = Some(ProxyServer::bind(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    Arc::clone(&self.public_auth),
                    Arc::clone(&self.upstream),
                )?);
            }

            if want_public
                && inner.tunnel.is_none()
                && (inner.public_state == RemoteTunnelState::Off
                    || (retry_failed_tunnel && inner.public_state == RemoteTunnelState::Failed))
            {
                let generation = self.public_generation.fetch_add(1, Ordering::SeqCst) + 1;
                inner.public_state = RemoteTunnelState::Starting;
                inner.public_url = None;
                inner.public_error = None;
                if let Some(listener) = inner.tunnel_listener.as_ref() {
                    bootstrap = Some((listener.port(), generation));
                }
            } else if !want_public {
                self.public_generation.fetch_add(1, Ordering::SeqCst);
                tunnel_to_stop = inner.tunnel.take();
                tunnel_listener_to_stop = inner.tunnel_listener.take();
                inner.public_state = RemoteTunnelState::Off;
                inner.public_url = None;
                inner.public_error = None;
            }
        }
        // Joining a tunnel while holding `inner` can deadlock with a URL or
        // exit callback that is already waiting for the same lock. Generation
        // was advanced above, so stale callbacks are harmless while shutdown
        // finishes outside the lock.
        if let Some(tunnel) = tunnel_to_stop {
            tunnel.stop();
        }
        if let Some(mut server) = lan_to_stop {
            server.stop();
        }
        if let Some(mut listener) = tunnel_listener_to_stop {
            listener.stop();
        }
        if let Some((port, generation)) = bootstrap {
            self.spawn_tunnel_worker(port, generation);
        }
        Ok(())
    }

    /// Tunnel bootstrap can take a while (first run downloads cloudflared),
    /// so it runs on its own worker and reports back into the service
    /// through the weak self-reference.
    fn spawn_tunnel_worker(&self, local_port: u16, generation: u64) {
        #[cfg(test)]
        if TUNNEL_WORKER_DISABLED.load(Ordering::SeqCst) {
            return; // Unit tests never download or spawn cloudflared.
        }
        let paths = self.paths.clone();
        let weak = self.self_weak.read().expect("self weak poisoned").clone();
        let worker = move || {
            let result = tunnel::ensure_binary(&paths).and_then(|binary| {
                let events: Arc<dyn TunnelEvents> = Arc::new(ServiceTunnelEvents {
                    service: weak.clone(),
                    generation,
                });
                TunnelProcess::spawn(&binary, local_port, events)
            });
            if let Some(service) = weak.upgrade() {
                service.complete_tunnel_bootstrap(result, generation);
            }
        };
        if let Err(error) = thread::Builder::new()
            .name("remote-tunnel-bootstrap".into())
            .spawn(worker)
        {
            log::error!("remote tunnel worker could not start: {error}");
            self.complete_tunnel_bootstrap(
                Err(AppError::io("remoteTunnelFailed", &error)),
                generation,
            );
        }
    }

    fn complete_tunnel_bootstrap(&self, result: AppResult<Arc<TunnelProcess>>, generation: u64) {
        // A dead child is dropped outside the inner lock: its Drop joins the
        // watcher threads, which may be blocked delivering on_exit on this
        // very lock.
        let mut dead_process = None;
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            if generation != self.public_generation.load(Ordering::SeqCst) {
                // A toggle raced the bootstrap; the spawned child belongs to
                // a dead generation. Dropping it (below) stops the process.
                dead_process = result.ok();
            } else {
                match result {
                    Ok(process) => {
                        // The exit watcher may already have fired for a
                        // child that died during bootstrap; never store a
                        // dead process, or the state would say Failed while
                        // retry considered a tunnel present and no-op'd
                        // forever.
                        if process.has_exited() {
                            dead_process = Some(process);
                        } else {
                            inner.tunnel = Some(process);
                        }
                    }
                    Err(error) => {
                        log::warn!("remote tunnel failed to start: {error}");
                        inner.public_state = RemoteTunnelState::Failed;
                        inner.public_error = Some(error);
                    }
                }
            }
        }
        drop(dead_process);
        self.notify();
    }

    fn tunnel_url_reported(&self, url: String, generation: u64) {
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            if generation != self.public_generation.load(Ordering::SeqCst)
                || inner.public_state != RemoteTunnelState::Starting
            {
                return;
            }
            inner.public_state = RemoteTunnelState::Running;
            inner.public_url = Some(url);
            inner.public_error = None;
        }
        self.notify();
    }

    fn tunnel_exited(&self, error: Option<AppError>, generation: u64) {
        {
            let mut inner = self.inner.lock().expect("remote poisoned");
            if generation != self.public_generation.load(Ordering::SeqCst) {
                return;
            }
            inner.tunnel = None;
            // Deliberate stops never report (the watcher returns early), so
            // any exit that reaches here — even a clean status 0 — is
            // unexpected and must surface as Failed with a usable retry.
            let error = error.unwrap_or_else(|| {
                AppError::new("remoteTunnelFailed").detail("cloudflared exited")
            });
            log::warn!("remote tunnel stopped unexpectedly: {error}");
            inner.public_state = RemoteTunnelState::Failed;
            inner.public_error = Some(error);
            inner.public_url = None;
        }
        self.notify();
    }

    /// Owner-facing snapshot; passwords included (see model docs).
    pub fn snapshot(&self) -> RemoteSnapshot {
        let inner = self.inner.lock().expect("remote poisoned");
        let service_ready = self.upstream.read().expect("upstream poisoned").is_some();
        let lan_url = inner.lan_server.as_ref().and_then(|server| {
            lan::primary_lan_ipv4().map(|ip| format!("http://{ip}:{}", server.port()))
        });
        RemoteSnapshot {
            master: inner.settings.master,
            service_ready,
            lan: RemoteLanSnapshot {
                enabled: inner.settings.lan_enabled,
                url: lan_url,
                password: inner.settings.lan_password.clone(),
            },
            public: RemotePublicSnapshot {
                enabled: inner.settings.public_enabled,
                state: inner.public_state,
                url: inner.public_url.clone(),
                password: inner.settings.public_password.clone(),
                error: inner.public_error.clone(),
            },
        }
    }

    /// Renders the QR code (SVG markup) for a scope's current URL.
    pub fn qr_svg(&self, scope: RemoteScope) -> AppResult<String> {
        let snapshot = self.snapshot();
        let url = match scope {
            RemoteScope::Lan => snapshot.lan.url,
            RemoteScope::Public => snapshot.public.url,
        }
        .ok_or_else(|| AppError::new("remoteUnavailable"))?;
        let code = qrcode::QrCode::new(url.as_bytes())
            .map_err(|error| AppError::new("remoteQrFailed").detail(error.to_string()))?;
        Ok(code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(192, 192)
            .quiet_zone(true)
            .build())
    }

    /// Stops every listener and the tunnel. State stays persisted, so the
    /// next launcher start restores the switches.
    pub fn shutdown(&self) {
        let (tunnel, lan_server, tunnel_listener) = {
            let mut inner = self.inner.lock().expect("remote poisoned");
            self.public_generation.fetch_add(1, Ordering::SeqCst);
            let resources = (
                inner.tunnel.take(),
                inner.lan_server.take(),
                inner.tunnel_listener.take(),
            );
            inner.public_state = RemoteTunnelState::Off;
            inner.public_url = None;
            inner.public_error = None;
            resources
        };
        if let Some(tunnel) = tunnel {
            tunnel.stop();
        }
        if let Some(mut server) = lan_server {
            server.stop();
        }
        if let Some(mut listener) = tunnel_listener {
            listener.stop();
        }
    }
}

/// Ensures the pinned, SHA-256-verified cloudflared binary exists and
/// returns its path. Used internally by the tunnel worker; also exposed for
/// the manual `remote_fetch_cloudflared` example.
pub fn ensure_cloudflared(paths: &ApplicationPaths) -> AppResult<std::path::PathBuf> {
    tunnel::ensure_binary(paths)
}

/// Test-only kill switch: tunnel workers never download or spawn
/// cloudflared in unit tests.
#[cfg(test)]
static TUNNEL_WORKER_DISABLED: AtomicBool = AtomicBool::new(false);

/// Extracts the `host:port` authority from the published web URL. IPv6
/// hosts keep their brackets so the result always parses as a SocketAddr.
fn upstream_authority(web_url: &str) -> AppResult<String> {
    let url = url::Url::parse(web_url).map_err(|_| AppError::new("invalidWebUrl"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::new("invalidWebUrl"))?;
    match url.host() {
        Some(url::Host::Domain(domain)) if domain.eq_ignore_ascii_case("localhost") => {
            Ok(format!("127.0.0.1:{port}"))
        }
        Some(url::Host::Domain(domain)) => Ok(format!("{domain}:{port}")),
        Some(url::Host::Ipv4(v4)) => Ok(format!("{v4}:{port}")),
        Some(url::Host::Ipv6(v6)) => Ok(format!("[{v6}]:{port}")),
        None => Err(AppError::new("invalidWebUrl")),
    }
}

/// Per-bootstrap events sink. Holds the service weakly so a torn-down
/// service never lingers because a cloudflared callback is in flight.
struct ServiceTunnelEvents {
    service: std::sync::Weak<RemoteService>,
    generation: u64,
}

impl TunnelEvents for ServiceTunnelEvents {
    fn on_url(&self, url: String) {
        if let Some(service) = self.service.upgrade() {
            service.tunnel_url_reported(url, self.generation);
        }
    }

    fn on_exit(&self, error: Option<AppError>) {
        if let Some(service) = self.service.upgrade() {
            service.tunnel_exited(error, self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (tempfile::TempDir, Arc<RemoteService>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        let service = RemoteService::new(paths).unwrap();
        (temp, service)
    }

    #[test]
    fn defaults_are_safe_and_passwords_valid() {
        let (_temp, service) = service();
        let snapshot = service.snapshot();
        assert!(!snapshot.master, "remote exposure is always opt-in");
        assert!(snapshot.lan.enabled);
        assert!(!snapshot.public.enabled);
        assert!(is_valid_password(&snapshot.lan.password));
        assert!(is_valid_password(&snapshot.public.password));
        assert!(snapshot.lan.url.is_none());
        assert_eq!(snapshot.public.state, RemoteTunnelState::Off);
    }

    #[test]
    fn settings_round_trip_across_restart() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        let first = RemoteService::new(paths.clone()).unwrap();
        first.set_master(true).unwrap();
        first.set_lan_enabled(true).unwrap();
        first.rotate_password(RemoteScope::Lan).unwrap();
        let password = first.snapshot().lan.password;
        first.shutdown();

        let second = RemoteService::new(paths).unwrap();
        let snapshot = second.snapshot();
        assert!(snapshot.master);
        assert!(snapshot.lan.enabled);
        assert_eq!(snapshot.lan.password, password);
        assert!(
            snapshot.lan.url.is_some(),
            "LAN listener restored: {snapshot:?}"
        );
    }

    #[test]
    fn master_off_stops_the_lan_listener() {
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        assert!(service.snapshot().lan.url.is_some());
        service.set_master(false).unwrap();
        assert!(service.snapshot().lan.url.is_none());
    }

    #[test]
    fn public_enable_requires_disclaimer_acknowledgement() {
        let (_temp, service) = service();
        let error = service.set_public_enabled(true, false).unwrap_err();
        assert_eq!(error.code, "remoteDisclaimerRequired");
        assert!(!service.snapshot().public.enabled);
    }

    #[test]
    fn rotation_changes_password_and_is_valid() {
        let (_temp, service) = service();
        let before = service.snapshot().lan.password;
        service.rotate_password(RemoteScope::Lan).unwrap();
        let after = service.snapshot().lan.password;
        assert!(is_valid_password(&after));
        // 1e8 space: a collision is possible but vanishingly unlikely; loop
        // once more if it happens rather than weakening the assertion.
        if after == before {
            service.rotate_password(RemoteScope::Lan).unwrap();
            assert_ne!(service.snapshot().lan.password, before);
        }
    }

    #[test]
    fn upstream_tracking_drives_service_ready() {
        let (_temp, service) = service();
        assert!(!service.snapshot().service_ready);
        service.set_upstream(Some("http://127.0.0.1:3080")).unwrap();
        assert!(service.snapshot().service_ready);
        service.set_upstream(None).unwrap();
        assert!(!service.snapshot().service_ready);
        assert!(service.set_upstream(Some("not a url")).is_err());
    }

    #[test]
    fn installing_change_listener_immediately_synchronizes_current_state() {
        let (_temp, service) = service();
        let notifications = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&notifications);
        service.set_change_listener(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn qr_requires_an_active_url() {
        let (_temp, service) = service();
        let error = service.qr_svg(RemoteScope::Lan).unwrap_err();
        assert_eq!(error.code, "remoteUnavailable");
        service.set_master(true).unwrap();
        if service.snapshot().lan.url.is_some() {
            let svg = service.qr_svg(RemoteScope::Lan).unwrap();
            assert!(svg.contains("<svg"), "{svg}");
        }
    }

    #[test]
    fn corrupt_passwords_on_disk_are_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path());
        std::fs::create_dir_all(&paths.remote_dir).unwrap();
        std::fs::write(
            &paths.remote_settings_file,
            r#"{"master":true,"lanEnabled":true,"publicEnabled":false,"lanPassword":"short","publicPassword":"12345678"}"#,
        )
        .unwrap();
        let service = RemoteService::new(paths).unwrap();
        let snapshot = service.snapshot();
        assert!(is_valid_password(&snapshot.lan.password));
        assert_eq!(snapshot.public.password, "12345678");
    }

    #[test]
    fn custom_password_is_validated_and_revokes_sessions() {
        let (_temp, service) = service();
        for bad in ["", "1234567", "123456789", "1234567a", " 2345678"] {
            let error = service
                .set_password(RemoteScope::Lan, bad.to_owned())
                .unwrap_err();
            assert_eq!(error.code, "remotePasswordInvalid", "{bad:?}");
        }
        // A live session must die when the password changes.
        let token = service
            .lan_auth
            .sessions
            .lock()
            .expect("sessions poisoned")
            .create();
        service
            .set_password(RemoteScope::Lan, "88889999".to_owned())
            .unwrap();
        assert_eq!(service.snapshot().lan.password, "88889999");
        assert!(
            !service
                .lan_auth
                .sessions
                .lock()
                .expect("sessions poisoned")
                .validate(&token),
            "sessions must be revoked on password change"
        );
    }

    #[test]
    fn failed_settings_write_does_not_change_live_switches_or_passwords() {
        let (_temp, service) = service();
        let before = service.snapshot();
        // A directory at the settings-file path makes atomic publication
        // fail without relying on platform-specific permission behavior.
        std::fs::create_dir_all(&service.paths.remote_settings_file).unwrap();

        assert!(service.set_master(true).is_err());
        assert!(
            service
                .set_password(RemoteScope::Lan, "88889999".to_owned())
                .is_err()
        );
        let after = service.snapshot();
        assert_eq!(after.master, before.master);
        assert_eq!(after.lan.password, before.lan.password);
        assert_eq!(
            *service.lan_auth.password.read().expect("password poisoned"),
            before.lan.password,
            "the proxy and owner snapshot must keep accepting/showing the same password"
        );
    }

    #[test]
    fn retry_requires_public_access_to_be_enabled() {
        let (_temp, service) = service();
        let error = service.retry_public_tunnel().unwrap_err();
        assert_eq!(error.code, "remoteUnavailable");
    }

    #[test]
    fn bootstrap_never_stores_a_process_that_already_exited() {
        // Regression: the exit watcher can fire before the bootstrap worker
        // stores the child. Storing the dead process left the scope Failed
        // with a tunnel present, so retry no-op'd forever.
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);
        let generation = service.public_generation.load(Ordering::SeqCst);

        // The exit report lands BEFORE the bootstrap completion.
        service.tunnel_exited(Some(AppError::new("remoteTunnelFailed")), generation);
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Failed);
        service.complete_tunnel_bootstrap(Ok(tunnel::TunnelProcess::exited_stub()), generation);
        assert!(
            service
                .inner
                .lock()
                .expect("remote poisoned")
                .tunnel
                .is_none(),
            "a process that already exited must never be stored"
        );

        // Retry re-bootstraps instead of no-op'ing on the stale process.
        service.retry_public_tunnel().unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);
    }

    #[test]
    fn clean_tunnel_exit_is_a_retryable_failure() {
        // cloudflared exiting with status 0 is still unexpected for a tunnel
        // that should outlive the session: it must surface as Failed and
        // stay retryable, while an explicit toggle off lands in Off.
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        let generation = service.public_generation.load(Ordering::SeqCst);

        service.tunnel_exited(None, generation);
        let snapshot = service.snapshot();
        assert_eq!(snapshot.public.state, RemoteTunnelState::Failed);
        assert_eq!(
            snapshot.public.error.map(|error| error.code).as_deref(),
            Some("remoteTunnelFailed")
        );

        service.retry_public_tunnel().unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);

        service.set_public_enabled(false, true).unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Off);
    }

    #[test]
    fn late_url_cannot_resurrect_an_exited_tunnel() {
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        let generation = service.public_generation.load(Ordering::SeqCst);

        service.tunnel_exited(Some(AppError::new("remoteTunnelFailed")), generation);
        service.tunnel_url_reported("https://too-late.trycloudflare.com".to_owned(), generation);
        let snapshot = service.snapshot();
        assert_eq!(snapshot.public.state, RemoteTunnelState::Failed);
        assert!(snapshot.public.url.is_none());
    }

    #[test]
    fn setter_failure_still_notifies_and_reflects_persisted_state() {
        // apply() failing (e.g. listener bind error) must not leave the UI
        // disagreeing with the persisted switch that the next launch will
        // restore.
        let (_temp, service) = service();
        let notifications = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&notifications);
        service.set_change_listener(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        service.apply_fails.store(true, Ordering::SeqCst);

        let error = service.set_master(true).unwrap_err();
        assert_eq!(error.code, "remoteListenFailed");
        assert!(
            notifications.load(Ordering::SeqCst) > 0,
            "the UI must be notified even when applying failed"
        );
        assert!(
            service.snapshot().master,
            "the snapshot mirrors the persisted switch"
        );
        service.apply_fails.store(false, Ordering::SeqCst);
    }

    #[test]
    fn upstream_authority_handles_ipv6_brackets_and_localhost() {
        let v6 = upstream_authority("http://[::1]:3000").unwrap();
        assert_eq!(v6, "[::1]:3000");
        assert!(
            v6.parse::<std::net::SocketAddr>().is_ok(),
            "bracketed IPv6 authority parses as a socket address"
        );
        assert_eq!(
            upstream_authority("http://127.0.0.1:3000").unwrap(),
            "127.0.0.1:3000"
        );
        assert_eq!(
            upstream_authority("http://localhost:3000").unwrap(),
            "127.0.0.1:3000"
        );
        assert_eq!(
            upstream_authority("http://LOCALHOST:3000").unwrap(),
            "127.0.0.1:3000"
        );
        assert!(upstream_authority("not a url").is_err());
    }

    #[test]
    fn retry_restarts_only_a_failed_tunnel() {
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        // The disabled worker leaves the bootstrap in Starting; retrying a
        // healthy in-flight bootstrap must be a no-op.
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);
        service.retry_public_tunnel().unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);

        // A failed tunnel goes back to Starting on retry.
        service.inner.lock().expect("remote poisoned").public_state = RemoteTunnelState::Failed;
        service.retry_public_tunnel().unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);
    }

    #[test]
    fn unrelated_settings_change_does_not_duplicate_a_starting_tunnel() {
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        let generation = service.public_generation.load(Ordering::SeqCst);

        service.set_lan_enabled(false).unwrap();
        assert_eq!(service.snapshot().public.state, RemoteTunnelState::Starting);
        assert_eq!(
            service.public_generation.load(Ordering::SeqCst),
            generation,
            "an unrelated apply must not launch a second download/process"
        );
    }

    #[test]
    fn unrelated_settings_change_does_not_restart_a_failed_tunnel() {
        TUNNEL_WORKER_DISABLED.store(true, Ordering::SeqCst);
        let (_temp, service) = service();
        service.set_master(true).unwrap();
        service.set_public_enabled(true, true).unwrap();
        let generation = service.public_generation.load(Ordering::SeqCst);
        service.tunnel_exited(
            Some(AppError::new("remoteTunnelFailed").detail("test failure")),
            generation,
        );

        service.set_lan_enabled(false).unwrap();
        let snapshot = service.snapshot();
        assert_eq!(snapshot.public.state, RemoteTunnelState::Failed);
        assert_eq!(
            snapshot
                .public
                .error
                .and_then(|error| error.safe_detail)
                .as_deref(),
            Some("test failure")
        );
        assert_eq!(
            service.public_generation.load(Ordering::SeqCst),
            generation,
            "an unrelated apply must preserve the failed tunnel generation"
        );
    }
}
