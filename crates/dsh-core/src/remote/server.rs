//! Authenticating reverse proxy in front of the loopback-only Harness web UI.
//!
//! The Harness web server binds 127.0.0.1. This proxy adds the remote-facing
//! guarded door: one listener per scope (LAN on the wildcard address, tunnel
//! on loopback for cloudflared), an 8-digit password login that issues an
//! opaque session cookie, and rate limiting. Newer Harness versions also
//! require their own launch-token exchange, which the proxy performs only on
//! the private loopback hop after its password check.
//!
//! Implementation notes:
//! - Plain `std` threads and blocking streams; no async runtime or web
//!   framework is pulled into the core crate. One thread per connection plus
//!   one copier per direction is ample for a personal single-device remote.
//! - Only the request head is parsed. After authentication the head is
//!   rewritten (Host, Cookie, Connection, Accept-Encoding) and request
//!   bodies flow upstream through a raw pipe. The response direction frames
//!   messages properly: streaming bodies pass through chunk by chunk, HTML
//!   pages are buffered briefly to inject the LAN browser polyfills
//!   (non-secure contexts lack `crypto.randomUUID`, which the dsh web client
//!   needs for every RPC id), and a `101` upgrade drops into a raw
//!   bidirectional pipe so WebSocket traffic flows untouched.
//! - Non-upgrade requests are forwarded with `Connection: close`: one
//!   upstream connection carries exactly one response, which keeps framing
//!   unambiguous. The multi-message response reader exists for `1xx`
//!   interims and the upgrade path, not for upstream keep-alive.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    AppError, AppResult,
    model::RemoteScope,
    remote::{
        Upstream,
        auth::{RateLimiter, SessionStore, password_matches},
    },
};

#[cfg(test)]
use crate::remote::UpstreamEndpoint;

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 128;

const LOGIN_PATH: &str = "/__dsh-remote/login";
const LOGOUT_PATH: &str = "/__dsh-remote/logout";
const BOOTSTRAP_PATH: &str = "/__dsh-remote/bootstrap";
const ICON_PATH: &str = "/__dsh-remote/icon.png";
const APP_ICON: &[u8] = include_bytes!("../../../../public/assets/logo.png");

/// Shared authentication state for one scope. Held by the owning
/// `RemoteService` and every live server of the scope, so password rotation
/// and session revocation apply to in-flight listeners immediately.
pub(crate) struct AuthState {
    pub(crate) scope: RemoteScope,
    pub(crate) password: RwLock<String>,
    pub(crate) sessions: Mutex<SessionStore>,
    /// Last Harness endpoint generation bootstrapped by each remote session.
    /// This is intentionally in memory: neither the upstream launch token nor
    /// its derived state is persisted by the launcher.
    bootstrapped: Mutex<HashMap<String, u64>>,
    pub(crate) limiter: Mutex<RateLimiter>,
    /// Weak handles to the live connection registries of this scope's
    /// servers, so revoking access also drops already-established
    /// connections (open WebSockets included), not just future logins.
    registries: Mutex<Vec<std::sync::Weak<StreamRegistry>>>,
}

impl AuthState {
    pub(crate) fn new(scope: RemoteScope, password: String) -> Arc<Self> {
        Arc::new(Self {
            scope,
            password: RwLock::new(password),
            sessions: Mutex::new(SessionStore::default()),
            bootstrapped: Mutex::new(HashMap::new()),
            limiter: Mutex::new(RateLimiter::default()),
            registries: Mutex::new(Vec::new()),
        })
    }

    fn cookie_name(&self) -> &'static str {
        match self.scope {
            RemoteScope::Lan => "dsh_remote_lan",
            RemoteScope::Public => "dsh_remote_pub",
        }
    }

    fn has_session(&self, token: &str) -> bool {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .validate(token)
    }

    fn is_bootstrapped(&self, token: &str, generation: u64) -> bool {
        self.bootstrapped
            .lock()
            .expect("bootstrap sessions poisoned")
            .get(token)
            .is_some_and(|existing| *existing == generation)
    }

    fn mark_bootstrapped(&self, token: &str, generation: u64) {
        self.bootstrapped
            .lock()
            .expect("bootstrap sessions poisoned")
            .insert(token.to_owned(), generation);
    }

    fn clear_bootstrapped(&self, token: &str) {
        self.bootstrapped
            .lock()
            .expect("bootstrap sessions poisoned")
            .remove(token);
    }

    /// Registers a server's connection registry for scope-wide revocation.
    fn track_registry(&self, registry: &Arc<StreamRegistry>) {
        self.registries
            .lock()
            .expect("registries poisoned")
            .push(std::sync::Arc::downgrade(registry));
    }

    /// Revokes every session AND terminates every live connection of the
    /// scope. Used on password change/rotation so a signed-in device is
    /// actually signed out — an open WebSocket must not survive it.
    pub(crate) fn revoke_all_connections(&self) {
        self.sessions.lock().expect("sessions poisoned").clear();
        self.bootstrapped
            .lock()
            .expect("bootstrap sessions poisoned")
            .clear();
        let mut registries = self.registries.lock().expect("registries poisoned");
        registries.retain(|weak| {
            if let Some(registry) = weak.upgrade() {
                registry.shutdown_all();
                true
            } else {
                false // Server gone; drop the dead weak handle.
            }
        });
    }

    /// Checks a password attempt under rate limiting. Returns Ok(token) on
    /// success, Err(false) on a wrong password, and Err(true) when locked out.
    fn attempt_login(&self, ip: IpAddr, candidate: &str) -> Result<String, bool> {
        {
            let mut limiter = self.limiter.lock().expect("limiter poisoned");
            if !limiter.allowed(ip, std::time::Instant::now()) {
                return Err(true);
            }
        }
        let expected = self.password.read().expect("password poisoned").clone();
        if password_matches(&expected, candidate) {
            self.limiter
                .lock()
                .expect("limiter poisoned")
                .record_success(ip);
            Ok(self.sessions.lock().expect("sessions poisoned").create())
        } else {
            self.limiter
                .lock()
                .expect("limiter poisoned")
                .record_failure(ip, std::time::Instant::now());
            Err(false)
        }
    }
}

struct StreamRegistry {
    streams: Mutex<Vec<(u64, TcpStream)>>,
    next_id: AtomicU64,
}

impl StreamRegistry {
    fn new() -> Self {
        Self {
            streams: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn register(&self, stream: &TcpStream) -> Option<u64> {
        let clone = stream.try_clone().ok()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.streams
            .lock()
            .expect("streams poisoned")
            .push((id, clone));
        Some(id)
    }

    fn unregister(&self, id: u64) {
        self.streams
            .lock()
            .expect("streams poisoned")
            .retain(|(existing, _)| *existing != id);
    }

    fn len(&self) -> usize {
        self.streams.lock().expect("streams poisoned").len()
    }

    fn shutdown_all(&self) {
        for (_, stream) in self.streams.lock().expect("streams poisoned").iter() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

/// A running proxy listener. `stop()` (also run on drop) terminates the
/// accept loop and every in-flight connection.
pub(crate) struct ProxyServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    registry: Arc<StreamRegistry>,
    listener: Option<JoinHandle<()>>,
}

impl ProxyServer {
    pub(crate) fn bind(
        host: IpAddr,
        auth: Arc<AuthState>,
        upstream: Arc<RwLock<Option<Upstream>>>,
    ) -> AppResult<Self> {
        let listener = TcpListener::bind(SocketAddr::new(host, 0))
            .map_err(|error| AppError::io("remoteListenFailed", &error))?;
        let addr = listener
            .local_addr()
            .map_err(|error| AppError::io("remoteListenFailed", &error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| AppError::io("remoteListenFailed", &error))?;
        let stop = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(StreamRegistry::new());
        auth.track_registry(&registry);
        let handle = {
            let stop = Arc::clone(&stop);
            let registry = Arc::clone(&registry);
            thread::Builder::new()
                .name(format!("remote-proxy-{}", auth.scope.as_str()))
                .spawn(move || accept_loop(listener, auth, upstream, stop, registry))
                .map_err(|error| AppError::io("remoteListenFailed", &error))?
        };
        Ok(Self {
            addr,
            stop,
            registry,
            listener: Some(handle),
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.addr.port()
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.registry.shutdown_all();
        // Prompt the nonblocking accept loop to observe the stop flag without
        // waiting for its next poll. A listener bound to an unspecified address
        // reports 0.0.0.0/:: as its local address, but those are bind-only
        // addresses and are not portable connect targets (notably on
        // Windows). Always wake wildcard listeners through loopback.
        let _ = TcpStream::connect(wake_address(self.addr));
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

fn wake_address(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port())
        }
        _ => address,
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl RemoteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteScope::Lan => "lan",
            RemoteScope::Public => "public",
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    auth: Arc<AuthState>,
    upstream: Arc<RwLock<Option<Upstream>>>,
    stop: Arc<AtomicBool>,
    registry: Arc<StreamRegistry>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(_) => break,
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Some platforms propagate O_NONBLOCK from the listener to accepted
        // sockets. Connection handlers intentionally use blocking I/O.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        if registry.len() >= MAX_CONNECTIONS * 2 {
            let mut stream = stream;
            let _ = write_response(
                &mut stream,
                503,
                "Service Unavailable",
                "text/plain; charset=utf-8",
                b"too many connections",
                &[],
            );
            continue;
        }
        let context = ConnectionContext {
            auth: Arc::clone(&auth),
            upstream: Arc::clone(&upstream),
            registry: Arc::clone(&registry),
        };
        let _ = thread::Builder::new()
            .name("remote-proxy-conn".into())
            .spawn(move || handle_connection(stream, context));
    }
}

struct ConnectionContext {
    auth: Arc<AuthState>,
    upstream: Arc<RwLock<Option<Upstream>>>,
    registry: Arc<StreamRegistry>,
}

struct RequestHead {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    /// Bytes read past the head terminator; part of the request body.
    leftover: Vec<u8>,
}

impl RequestHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn path(&self) -> &str {
        self.target.split(['?', '#']).next().unwrap_or("/")
    }

    fn is_websocket(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }

    fn session_token(&self, cookie_name: &str) -> Option<String> {
        let prefix = format!("{cookie_name}=");
        self.header("cookie")?.split(';').find_map(|pair| {
            let pair = pair.trim();
            pair.strip_prefix(&prefix).map(str::to_owned)
        })
    }
}

fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<RequestHead>> {
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(error) => return Err(error),
        }
    };
    let _ = stream.set_read_timeout(None);
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let leftover = buffer.split_off(head_end + 4);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    if !target.starts_with('/') {
        // Absolute-form targets are a proxy-abuse vector; browsers always use
        // origin-form, so reject everything else outright.
        return Ok(None);
    }
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Ok(Some(RequestHead {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
        leftover,
    }))
}

fn handle_connection(mut client: TcpStream, context: ConnectionContext) {
    let Some(registration) = context.registry.register(&client) else {
        return;
    };
    let _ = handle_connection_inner(&mut client, &context);
    context.registry.unregister(registration);
}

fn handle_connection_inner(
    client: &mut TcpStream,
    context: &ConnectionContext,
) -> std::io::Result<()> {
    let peer_ip = client
        .peer_addr()
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let Some(head) = read_head(client)? else {
        return Ok(());
    };
    let login_ip = login_source_ip(&head, peer_ip, context.auth.scope);
    let path = head.path().to_owned();
    match (head.method.as_str(), path.as_str()) {
        ("GET", ICON_PATH) => write_response(client, 200, "OK", "image/png", APP_ICON, &[]),
        ("GET", LOGIN_PATH) => {
            if authenticated(&head, &context.auth) {
                return redirect(client, "/");
            }
            let failed = head.target.contains("error=1");
            serve_login_page(client, &head, failed, None)
        }
        ("POST", LOGIN_PATH) => handle_login(client, head, login_ip, &context.auth),
        ("GET" | "POST", LOGOUT_PATH) => {
            if let Some(token) = head.session_token(context.auth.cookie_name()) {
                context
                    .auth
                    .sessions
                    .lock()
                    .expect("sessions poisoned")
                    .revoke(&token);
                context.auth.clear_bootstrapped(&token);
            }
            let cookie = expired_cookie(context.auth.cookie_name());
            redirect_with_cookie(client, LOGIN_PATH, &cookie)
        }
        ("GET", BOOTSTRAP_PATH) => {
            let Some(session) = authenticated_session(&head, &context.auth) else {
                return redirect(client, LOGIN_PATH);
            };
            let upstream = context.upstream.read().expect("upstream poisoned").clone();
            let Some(upstream) = upstream else {
                return proxy(client, head, context);
            };
            let Some(target) = upstream.endpoint.bootstrap_target.clone() else {
                return redirect(client, "/");
            };
            // The private launch token is substituted only on the loopback
            // hop. It never appears in the remote URL, redirect, or QR code.
            context
                .auth
                .mark_bootstrapped(&session, upstream.generation);
            let result = proxy_to(client, head, context, upstream, Some(target));
            if result.is_err() {
                // A connection failure must remain retryable on the next
                // navigation instead of pinning this session to a dead hop.
                context.auth.clear_bootstrapped(&session);
            }
            result
        }
        _ => {
            let Some(session) = authenticated_session(&head, &context.auth) else {
                if head.method == "GET" {
                    return redirect(client, LOGIN_PATH);
                }
                return write_response(
                    client,
                    401,
                    "Unauthorized",
                    "text/plain; charset=utf-8",
                    b"authentication required",
                    &[],
                );
            };
            if head.method == "GET" {
                let bootstrap_generation = context
                    .upstream
                    .read()
                    .expect("upstream poisoned")
                    .as_ref()
                    .and_then(|upstream| {
                        upstream
                            .endpoint
                            .bootstrap_target
                            .as_ref()
                            .map(|_| upstream.generation)
                    });
                if bootstrap_generation
                    .is_some_and(|generation| !context.auth.is_bootstrapped(&session, generation))
                {
                    return redirect(client, BOOTSTRAP_PATH);
                }
            }
            proxy(client, head, context)
        }
    }
}

/// cloudflared is the only process that can reach the public listener, so
/// its Cloudflare-injected visitor address is the meaningful rate-limit key.
/// LAN traffic never trusts forwarded headers, and a missing or malformed
/// public header safely falls back to the socket peer.
fn login_source_ip(head: &RequestHead, peer_ip: IpAddr, scope: RemoteScope) -> IpAddr {
    if scope == RemoteScope::Public && peer_ip.is_loopback() {
        return head
            .header("cf-connecting-ip")
            .and_then(|value| value.parse().ok())
            .unwrap_or(peer_ip);
    }
    peer_ip
}

fn authenticated(head: &RequestHead, auth: &AuthState) -> bool {
    authenticated_session(head, auth).is_some()
}

fn authenticated_session(head: &RequestHead, auth: &AuthState) -> Option<String> {
    head.session_token(auth.cookie_name())
        .filter(|token| auth.has_session(token))
}

fn handle_login(
    client: &mut TcpStream,
    head: RequestHead,
    peer_ip: IpAddr,
    auth: &AuthState,
) -> std::io::Result<()> {
    let Some(body) = read_form_body(client, &head)? else {
        return write_response(
            client,
            400,
            "Bad Request",
            "text/plain; charset=utf-8",
            b"invalid login form",
            &[],
        );
    };
    let candidate = form_value(&body, "password").unwrap_or_default();
    match auth.attempt_login(peer_ip, &candidate) {
        Ok(token) => {
            let cookie = session_cookie(auth.cookie_name(), &token, auth.scope);
            redirect_with_cookie(client, "/", &cookie)
        }
        Err(true) => {
            let zh = prefers_chinese(&head);
            let page = login_page(zh, false, Some(locked_copy(zh)));
            write_response(
                client,
                429,
                "Too Many Requests",
                "text/html; charset=utf-8",
                page.as_bytes(),
                &[("retry-after", "60")],
            )
        }
        Err(false) => redirect(client, "/__dsh-remote/login?error=1"),
    }
}

fn read_form_body(client: &mut TcpStream, head: &RequestHead) -> std::io::Result<Option<String>> {
    let length: usize = match head.header("content-length").and_then(|v| v.parse().ok()) {
        Some(length) if length <= MAX_BODY_BYTES => length,
        _ => return Ok(None),
    };
    let mut body = head.leftover.clone();
    body.truncate(length.min(body.len()));
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    while body.len() < length {
        let mut chunk = [0_u8; 4096];
        match client.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(count) => body.extend_from_slice(&chunk[..count]),
            Err(error) => return Err(error),
        }
    }
    let _ = client.set_read_timeout(None);
    Ok(String::from_utf8(body).ok())
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => out.push(byte),
                    Err(_) => out.push(bytes[index]),
                }
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn proxy(
    client: &mut TcpStream,
    head: RequestHead,
    context: &ConnectionContext,
) -> std::io::Result<()> {
    let upstream = context.upstream.read().expect("upstream poisoned").clone();
    let Some(upstream) = upstream else {
        return write_response(
            client,
            503,
            "Service Unavailable",
            "text/plain; charset=utf-8",
            b"desktop service is not running",
            &[],
        );
    };
    proxy_to(client, head, context, upstream, None)
}

fn proxy_to(
    client: &mut TcpStream,
    mut head: RequestHead,
    context: &ConnectionContext,
    upstream_endpoint: Upstream,
    target: Option<String>,
) -> std::io::Result<()> {
    if let Some(target) = target {
        head.target = target;
    }
    let authority = upstream_endpoint.endpoint.authority;
    let mut upstream = TcpStream::connect_timeout(
        &authority
            .parse::<SocketAddr>()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        CONNECT_TIMEOUT,
    )?;
    let websocket = head.is_websocket();
    let request_is_head = head.method.eq_ignore_ascii_case("HEAD");
    upstream.write_all(rewritten_head(&head, &authority, websocket).as_bytes())?;
    if !head.leftover.is_empty() {
        upstream.write_all(&head.leftover)?;
    }
    let registration = context.registry.register(&upstream);
    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let copier = match thread::Builder::new()
        .name("remote-proxy-request".into())
        .spawn(move || {
            // Request bodies (and every later keep-alive request on this
            // connection) flow upstream untouched.
            let _ = std::io::copy(&mut client_reader, &mut upstream_writer);
            let _ = upstream_writer.shutdown(Shutdown::Both);
            let _ = client_reader.shutdown(Shutdown::Both);
        }) {
        Ok(copier) => copier,
        Err(error) => {
            let _ = upstream.shutdown(Shutdown::Both);
            let _ = client.shutdown(Shutdown::Both);
            if let Some(id) = registration {
                context.registry.unregister(id);
            }
            return Err(error);
        }
    };
    let result = forward_responses(&mut upstream, client, request_is_head);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
    let _ = copier.join();
    if let Some(id) = registration {
        context.registry.unregister(id);
    }
    result
}

// ---------------------------------------------------------------------------
// Response forwarding
// ---------------------------------------------------------------------------
//
// The upstream side is a proper (small) HTTP/1.1 reader: responses are
// framed by status, Transfer-Encoding, and Content-Length so streaming
// bodies (SSE) pass through chunk by chunk with minimal latency, `1xx`
// interims and a keep-alive-tolerant upstream are handled, and `101`
// upgrades drop into a raw bidirectional pipe. `text/html` responses are
// buffered (they are tiny) so the browser polyfill block can be injected —
// see API_POLYFILL below.

const HTML_BODY_CAP: u64 = 16 * 1024 * 1024;

/// Browser polyfills for non-secure LAN contexts (`http://192.168.x.x`):
/// `crypto.randomUUID` is only exposed in secure contexts, yet the dsh web
/// client mints every RPC id with it — without this shim every API call
/// throws and the UI renders empty workspaces. `AbortSignal.any` is missing
/// on several Android vendor WebViews. Both shims only install when the
/// native API is absent.
const API_POLYFILL: &str = concat!(
    "<script data-dsh-remote-polyfill=\"1\">(function(){try{",
    "var c=window.crypto;if(c&&!c.randomUUID&&c.getRandomValues){c.randomUUID=function(){var b=c.getRandomValues(new Uint8Array(16));b[6]=(b[6]&15)|64;b[8]=(b[8]&63)|128;var h=\"0123456789abcdef\",s=\"\",i=0;for(;i<16;i++)s+=h[b[i]>>4]+h[b[i]&15];return s.slice(0,8)+\"-\"+s.slice(8,12)+\"-\"+s.slice(12,16)+\"-\"+s.slice(16,20)+\"-\"+s.slice(20)}}",
    "if(window.AbortSignal&&!AbortSignal.any){AbortSignal.any=function(list){var ac=new AbortController();for(var i=0;i<list.length;i++){var s=list[i];if(!s)continue;if(s.aborted){ac.abort(s.reason);break}s.addEventListener(\"abort\",function(){ac.abort(s.reason)},{once:true})}return ac.signal}}",
    "}catch(e){}})();</script>",
);

// NOTE: the dsh web client also gates its settings store behind a loopback
// `location.hostname` check (`isLoopback`). Spoofing that via a top-level
// `let location` Proxy binding is deliberately NOT done: a global lexical
// `location` turns any classic script that happens to declare `location` at
// top level (some dsh plugin bundles do) into a SyntaxError that kills the
// whole page. Settings stay desktop-side; remote browsers run the client's
// process-local mode, as upstream intends.

/// Buffered reader over the upstream socket. Bytes read past a message
/// boundary stay buffered for the next step of the state machine.
struct Pipeline<'a> {
    stream: &'a mut TcpStream,
    buffer: Vec<u8>,
}

impl Pipeline<'_> {
    fn fill_some(&mut self) -> std::io::Result<usize> {
        let mut chunk = [0_u8; 16384];
        let count = self.stream.read(&mut chunk)?;
        self.buffer.extend_from_slice(&chunk[..count]);
        Ok(count)
    }

    /// Reads one head terminated by CRLFCRLF; the terminator is included in
    /// the returned raw bytes. None on EOF or an oversized head.
    fn read_head(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(position) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(Some(self.buffer.drain(..position + 4).collect()));
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return Ok(None);
            }
            if self.fill_some()? == 0 {
                return Ok(None);
            }
        }
    }

    /// Reads one CRLF-terminated line (terminator included).
    fn read_line(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(position) = self.buffer.iter().position(|b| *b == b'\n') {
                return Ok(self.buffer.drain(..=position).collect());
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "oversized line",
                ));
            }
            if self.fill_some()? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof in line",
                ));
            }
        }
    }

    /// Moves exactly `count` bytes into `out`.
    fn take_into(&mut self, mut count: u64, out: &mut Vec<u8>) -> std::io::Result<()> {
        while count > 0 {
            if self.buffer.is_empty() && self.fill_some()? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof in body",
                ));
            }
            let take = (count as usize).min(self.buffer.len());
            out.extend_from_slice(&self.buffer.drain(..take).collect::<Vec<_>>());
            count -= take as u64;
        }
        Ok(())
    }

    /// Streams exactly `count` bytes to the client.
    fn forward_into(&mut self, client: &mut TcpStream, mut count: u64) -> std::io::Result<()> {
        while count > 0 {
            if self.buffer.is_empty() && self.fill_some()? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof in body",
                ));
            }
            let take = (count as usize).min(self.buffer.len());
            client.write_all(&self.buffer[..take])?;
            self.buffer.drain(..take);
            count -= take as u64;
        }
        Ok(())
    }
}

struct ResponseHead {
    raw: Vec<u8>,
    status: u16,
    headers: Vec<(String, String)>,
}

impl ResponseHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn parse_response_head(raw: Vec<u8>) -> Option<ResponseHead> {
    let text = String::from_utf8_lossy(&raw).into_owned();
    let mut lines = text.split("\r\n");
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Some(ResponseHead {
        raw,
        status,
        headers,
    })
}

enum Framing {
    Chunked,
    Length(u64),
    NoBody,
    /// Delimited by the connection closing (or an upgrade handled earlier).
    Close,
}

fn framing(head: &ResponseHead) -> Framing {
    if ((100..200).contains(&head.status) && head.status != 101)
        || head.status == 204
        || head.status == 304
    {
        return Framing::NoBody;
    }
    if head
        .header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return Framing::Chunked;
    }
    if let Some(length) = head
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Framing::Length(length);
    }
    Framing::Close
}

/// HTML bodies are buffered and injected only when they arrive uncompressed
/// (the request rewrite strips Accept-Encoding, so this is the norm), are
/// framed explicitly, and stay under a generous cap.
fn injectable(head: &ResponseHead) -> bool {
    let html = head
        .header("content-type")
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"));
    let identity = head.header("content-encoding").is_none();
    let bounded = match framing(head) {
        Framing::Chunked => true,
        Framing::Length(length) => length <= HTML_BODY_CAP,
        _ => false,
    };
    html && identity && bounded
}

/// Inserts the polyfill scripts immediately after the opening `<head>` tag
/// so they run before every application script. Unrecognized markup is
/// passed through untouched.
fn inject_polyfill(body: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    let lower = text.to_ascii_lowercase();
    let Some(head_start) = lower.find("<head") else {
        return body.to_vec();
    };
    let Some(close) = lower[head_start..].find('>') else {
        return body.to_vec();
    };
    let at = head_start + close + 1;
    let mut out = String::with_capacity(text.len() + API_POLYFILL.len());
    out.push_str(&text[..at]);
    out.push_str(API_POLYFILL);
    out.push_str(&text[at..]);
    out.into_bytes()
}

/// Rebuilds the response head for a buffered body: Transfer-Encoding and the
/// old Content-Length are replaced by the new exact length.
fn rewritten_response_head(head: &ResponseHead, body_len: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(&head.raw);
    let mut out = String::with_capacity(head.raw.len() + 32);
    let mut lines = text.split("\r\n");
    if let Some(status_line) = lines.next() {
        out.push_str(status_line);
        out.push_str("\r\n");
    }
    for line in lines {
        if line.is_empty() {
            break;
        }
        let name = line
            .split(':')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if name == "transfer-encoding" || name == "content-length" {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Content-Length: {body_len}\r\n\r\n"));
    out.into_bytes()
}

/// Buffers one complete body, de-chunking when necessary.
fn read_body_full(pipe: &mut Pipeline, head: &ResponseHead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    match framing(head) {
        Framing::Length(length) => pipe.take_into(length, &mut body)?,
        Framing::Chunked => loop {
            let line = pipe.read_line()?;
            let size_text = String::from_utf8_lossy(&line);
            let size = u64::from_str_radix(size_text.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            if size == 0 {
                // Trailer section: read lines up to the terminating empty one.
                loop {
                    let trailer = pipe.read_line()?;
                    if trailer == b"\r\n" {
                        break;
                    }
                }
                break;
            }
            if size > HTML_BODY_CAP.saturating_sub(body.len() as u64) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "html body too large",
                ));
            }
            pipe.take_into(size, &mut body)?;
            // Chunk data is followed by a CRLF separator.
            let mut separator = Vec::with_capacity(2);
            pipe.take_into(2, &mut separator)?;
            if separator != b"\r\n" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid chunk separator",
                ));
            }
        },
        _ => {}
    }
    Ok(body)
}

fn forward_chunked(pipe: &mut Pipeline, client: &mut TcpStream) -> std::io::Result<()> {
    loop {
        let line = pipe.read_line()?;
        client.write_all(&line)?;
        let size_text = String::from_utf8_lossy(&line);
        let size = u64::from_str_radix(size_text.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if size == 0 {
            loop {
                let trailer = pipe.read_line()?;
                client.write_all(&trailer)?;
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }
        pipe.forward_into(client, size + 2)?;
    }
}

fn forward_responses(
    upstream: &mut TcpStream,
    client: &mut TcpStream,
    request_is_head: bool,
) -> std::io::Result<()> {
    let mut pipe = Pipeline {
        stream: upstream,
        buffer: Vec::new(),
    };
    loop {
        let _ = pipe.stream.set_read_timeout(Some(HEAD_TIMEOUT));
        let Some(raw) = pipe.read_head()? else {
            return Ok(());
        };
        let _ = pipe.stream.set_read_timeout(None);
        let Some(head) = parse_response_head(raw) else {
            return Ok(());
        };
        if head.status == 101 {
            // Upgrade confirmed: everything after the head is an opaque
            // bidirectional byte stream (WebSocket frames).
            client.write_all(&head.raw)?;
            if !pipe.buffer.is_empty() {
                client.write_all(&pipe.buffer)?;
                pipe.buffer.clear();
            }
            let _ = std::io::copy(pipe.stream, client);
            return Ok(());
        }
        if request_is_head && head.status >= 200 {
            // Content-Length on HEAD describes the corresponding GET body;
            // no bytes follow it. Never wait for or inject a nonexistent
            // HTML body.
            client.write_all(&head.raw)?;
            return Ok(());
        }
        if injectable(&head) {
            let body = read_body_full(&mut pipe, &head)?;
            let body = inject_polyfill(&body);
            client.write_all(&rewritten_response_head(&head, body.len()))?;
            client.write_all(&body)?;
            continue;
        }
        client.write_all(&head.raw)?;
        match framing(&head) {
            Framing::NoBody => {}
            Framing::Length(length) => pipe.forward_into(client, length)?,
            Framing::Chunked => forward_chunked(&mut pipe, client)?,
            Framing::Close => {
                if !pipe.buffer.is_empty() {
                    client.write_all(&pipe.buffer)?;
                    pipe.buffer.clear();
                }
                let _ = std::io::copy(pipe.stream, client);
                return Ok(());
            }
        }
    }
}

/// Rewrites the head for the upstream: Host, Origin, and Referer point at
/// the loopback service (the upstream 403s host-sensitive APIs for
/// non-loopback origins), our session cookie and hop-by-hop proxy headers
/// are stripped, and Accept-Encoding is removed so HTML responses arrive
/// uncompressed and can be buffered for polyfill injection. Upgrade requests
/// keep their Connection and Upgrade headers verbatim.
fn rewritten_head(head: &RequestHead, authority: &str, websocket: bool) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&head.method);
    out.push(' ');
    out.push_str(&head.target);
    out.push_str(" HTTP/1.1\r\n");
    out.push_str("Host: ");
    out.push_str(authority);
    out.push_str("\r\n");
    for (name, value) in &head.headers {
        match name.as_str() {
            "host" | "proxy-connection" | "proxy-authorization" | "accept-encoding" => {}
            "cookie" => {
                if let Some(cookie) = upstream_cookie(value) {
                    out.push_str("Cookie: ");
                    out.push_str(&cookie);
                    out.push_str("\r\n");
                }
            }
            // The upstream forbids host-sensitive APIs (e.g. the native
            // directory picker) for non-loopback origins with a bare 403.
            // The connection genuinely is loopback — the browser's LAN
            // origin only reflects our proxy address — so point Origin and
            // Referer at the upstream authority.
            "origin" => {
                out.push_str("Origin: http://");
                out.push_str(authority);
                out.push_str("\r\n");
            }
            "referer" => {
                out.push_str("Referer: http://");
                out.push_str(authority);
                out.push_str("/\r\n");
            }
            "connection" if !websocket => out.push_str("Connection: close\r\n"),
            _ => {
                out.push_str(name);
                out.push_str(": ");
                out.push_str(value);
                out.push_str("\r\n");
            }
        }
    }
    if !websocket && head.header("connection").is_none() {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out
}

/// Removes the launcher's authentication cookies while preserving cookies
/// owned by the Harness web app or its plugins.
fn upstream_cookie(value: &str) -> Option<String> {
    let cookies = value
        .split(';')
        .map(str::trim)
        .filter(|pair| {
            let name = pair.split_once('=').map_or(*pair, |(name, _)| name).trim();
            name != "dsh_remote_lan" && name != "dsh_remote_pub"
        })
        .filter(|pair| !pair.is_empty())
        .collect::<Vec<_>>();
    (!cookies.is_empty()).then(|| cookies.join("; "))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)
}

fn redirect(stream: &mut TcpStream, location: &str) -> std::io::Result<()> {
    write_response(
        stream,
        302,
        "Found",
        "text/plain; charset=utf-8",
        b"",
        &[("location", location)],
    )
}

fn redirect_with_cookie(
    stream: &mut TcpStream,
    location: &str,
    cookie: &str,
) -> std::io::Result<()> {
    write_response(
        stream,
        302,
        "Found",
        "text/plain; charset=utf-8",
        b"",
        &[("location", location), ("set-cookie", cookie)],
    )
}

fn session_cookie(name: &str, token: &str, scope: RemoteScope) -> String {
    let secure = match scope {
        // The public listener only ever serves the cloudflared edge, which
        // terminates TLS; the LAN listener speaks plain HTTP.
        RemoteScope::Public => "; Secure",
        RemoteScope::Lan => "",
    };
    format!("{name}={token}; Path=/; HttpOnly; SameSite=Strict{secure}")
}

fn expired_cookie(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

fn prefers_chinese(head: &RequestHead) -> bool {
    head.header("accept-language")
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("zh"))
}

struct LoginCopy<'a> {
    subtitle: &'a str,
    label: &'a str,
    submit: &'a str,
    failed: &'a str,
}

fn login_copy(zh: bool) -> LoginCopy<'static> {
    if zh {
        LoginCopy {
            subtitle: "远程访问DSH",
            label: "连接密码",
            submit: "连接",
            failed: "密码错误，请重试",
        }
    } else {
        LoginCopy {
            subtitle: "Remote access to DSH",
            label: "Connection password",
            submit: "Connect",
            failed: "Incorrect password, try again",
        }
    }
}

fn locked_copy(zh: bool) -> &'static str {
    if zh {
        "尝试次数过多，请 60 秒后再试"
    } else {
        "Too many attempts; retry in 60 seconds"
    }
}

fn login_page(zh: bool, failed: bool, locked: Option<&str>) -> String {
    let copy = login_copy(zh);
    let notice = match (locked, failed) {
        (Some(message), _) => message,
        (None, true) => copy.failed,
        (None, false) => "",
    };
    format!(
        r##"<!doctype html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#f7f7f9" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#17181c" media="(prefers-color-scheme: dark)">
<title>DSH Launcher · {subtitle}</title>
<link rel="icon" type="image/png" href="{icon_path}">
<style>
:root {{ color-scheme: light; --bg: #f7f7f9; --surface: #fff; --text: #1f1f22; --muted: #8a8a92; --border: #e7e7e9; --border-strong: #d7d7db; --brand: #2f6cff; --brand-hover: #235be2; --brand-soft: #dfe7fb; --danger: #d63730; --shadow: 0 1px 3px rgb(0 0 0 / 7%); }}
* {{ box-sizing: border-box; }}
body {{ margin: 0; min-height: 100vh; min-height: 100dvh; display: grid; place-items: center; padding: max(28px, env(safe-area-inset-top)) 24px max(28px, env(safe-area-inset-bottom)); background: var(--bg); color: var(--text); font-family: Inter, -apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif; }}
.login {{ width: min(360px, 100%); }}
.identity {{ display: flex; flex-direction: column; align-items: center; margin-bottom: 34px; text-align: center; }}
.app-icon {{ width: 76px; height: 76px; margin-bottom: 18px; border-radius: 20px; box-shadow: 0 5px 16px rgb(0 0 0 / 14%); object-fit: contain; }}
h1 {{ margin: 0; font-size: 27px; font-weight: 720; letter-spacing: -.025em; line-height: 1.15; }}
.subtitle {{ margin: 9px 0 0; color: var(--muted); font-size: 14px; line-height: 1.5; }}
form {{ display: grid; gap: 12px; }}
.field-label {{ position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0, 0, 0, 0); clip-path: inset(50%); white-space: nowrap; }}
input {{ width: 100%; height: 48px; padding: 0 16px; border: 1px solid var(--border-strong); border-radius: 8px; outline: 0; background: var(--surface); box-shadow: var(--shadow); color: var(--text); font: inherit; font-size: 17px; letter-spacing: 2px; text-align: center; transition: border-color 150ms ease, box-shadow 150ms ease; }}
input::placeholder {{ color: var(--muted); letter-spacing: 0; }}
input:focus {{ border-color: var(--brand); box-shadow: 0 0 0 2px var(--brand-soft); }}
button {{ height: 44px; padding: 0 16px; border: 0; border-radius: 8px; background: var(--brand); box-shadow: 0 2px 5px rgb(47 108 255 / 23%); color: #fff; font: inherit; font-size: 14px; font-weight: 650; cursor: pointer; transition: background 150ms ease, transform 100ms ease; }}
button:hover {{ background: var(--brand-hover); }}
button:active {{ transform: translateY(1px); }}
button:focus-visible {{ outline: 2px solid var(--brand); outline-offset: 2px; }}
.error {{ order: -1; margin: 0 0 2px; color: var(--danger); font-size: 13px; line-height: 1.4; text-align: center; }}
.error:empty {{ display: none; }}
@media (prefers-color-scheme: dark) {{ :root {{ color-scheme: dark; --bg: #17181c; --surface: #222328; --text: #f2f2f4; --muted: #a5a5ad; --border: #33343a; --border-strong: #44454d; --brand: #5d89ff; --brand-hover: #7399ff; --brand-soft: #283657; --danger: #ff7770; --shadow: 0 1px 3px rgb(0 0 0 / 30%); }} }}
@media (max-height: 520px) {{ .identity {{ margin-bottom: 24px; }} .app-icon {{ width: 64px; height: 64px; margin-bottom: 14px; border-radius: 17px; }} }}
@media (prefers-reduced-motion: reduce) {{ input, button {{ transition: none; }} }}
</style>
</head>
<body>
<main class="login">
<header class="identity">
<img class="app-icon" src="{icon_path}" width="76" height="76" alt="">
<h1>DSH Launcher</h1>
<p class="subtitle">{subtitle}</p>
</header>
<form method="post" action="/__dsh-remote/login" autocomplete="off">
<label class="field-label" for="password">{label}</label>
<input id="password" type="password" name="password" inputmode="numeric" pattern="[0-9]*" maxlength="8" required aria-describedby="login-error" placeholder="{label}" autofocus>
<button type="submit">{submit}</button>
<p class="error" id="login-error" role="alert">{notice}</p>
</form>
</main>
</body>
</html>"##,
        lang = if zh { "zh-CN" } else { "en" },
        subtitle = copy.subtitle,
        icon_path = ICON_PATH,
        notice = notice,
        label = copy.label,
        submit = copy.submit,
    )
}

fn serve_login_page(
    client: &mut TcpStream,
    head: &RequestHead,
    failed: bool,
    locked: Option<&str>,
) -> std::io::Result<()> {
    let zh = prefers_chinese(head);
    let page = login_page(zh, failed, locked);
    write_response(
        client,
        200,
        "OK",
        "text/html; charset=utf-8",
        page.as_bytes(),
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::auth;
    use std::{net::Ipv4Addr, sync::Arc, thread, time::Instant};

    #[test]
    fn wildcard_listener_wakes_through_loopback() {
        let port = 43123;
        assert_eq!(
            wake_address(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        );
        assert_eq!(
            wake_address(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
        );
    }

    #[test]
    fn wildcard_listener_stop_returns_within_a_bounded_time() {
        let auth = AuthState::new(RemoteScope::Lan, "12345678".to_owned());
        let upstream = Arc::new(RwLock::new(None));
        let mut server = ProxyServer::bind(IpAddr::V4(Ipv4Addr::UNSPECIFIED), auth, upstream)
            .expect("wildcard proxy binds");

        let started = Instant::now();
        server.stop();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "proxy stop must not wait indefinitely"
        );
    }

    struct FakeUpstream {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeUpstream {
        /// Records request heads and replies with a fixed body; upgrade
        /// requests get a raw echo channel after a synthetic 101.
        fn spawn(seen: Arc<Mutex<Vec<String>>>) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    let seen = Arc::clone(&seen);
                    thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buffer = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut chunk) {
                                Ok(0) | Err(_) => return,
                                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                            }
                        }
                        let head = String::from_utf8_lossy(&buffer).into_owned();
                        let upgrade = head.to_ascii_lowercase().contains("upgrade: websocket");
                        seen.lock().expect("seen poisoned").push(head);
                        if upgrade {
                            let _ = stream.write_all(
                                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
                            );
                            let _ = stream.set_read_timeout(None);
                            // Raw echo: everything received goes back.
                            let mut echo = [0_u8; 1024];
                            loop {
                                match stream.read(&mut echo) {
                                    Ok(0) | Err(_) => break,
                                    Ok(count) => {
                                        if stream.write_all(&echo[..count]).is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        } else {
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
                            );
                        }
                    });
                }
            });
            Self {
                addr,
                stop,
                handle: Some(handle),
            }
        }

        fn authority(&self) -> String {
            self.addr.to_string()
        }
    }

    impl Drop for FakeUpstream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    struct TestProxy {
        server: ProxyServer,
        auth: Arc<AuthState>,
        _upstream: FakeUpstream,
    }

    fn upstream_config(
        authority: String,
        bootstrap_target: Option<&str>,
        generation: u64,
    ) -> Arc<RwLock<Option<Upstream>>> {
        Arc::new(RwLock::new(Some(Upstream::new(
            UpstreamEndpoint {
                authority,
                bootstrap_target: bootstrap_target.map(str::to_owned),
            },
            generation,
        ))))
    }

    fn start_proxy(password: &str, seen: &Arc<Mutex<Vec<String>>>) -> TestProxy {
        start_proxy_for(RemoteScope::Lan, password, seen)
    }

    fn start_proxy_for(
        scope: RemoteScope,
        password: &str,
        seen: &Arc<Mutex<Vec<String>>>,
    ) -> TestProxy {
        let upstream = FakeUpstream::spawn(Arc::clone(seen));
        let authority = upstream_config(upstream.authority(), None, 1);
        let auth = AuthState::new(scope, password.to_owned());
        let server = ProxyServer::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&auth),
            authority,
        )
        .expect("proxy binds");
        TestProxy {
            server,
            auth,
            _upstream: upstream,
        }
    }

    fn request(proxy: &TestProxy, raw: &str) -> String {
        let mut stream = TcpStream::connect(proxy.server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => response.extend_from_slice(&chunk[..count]),
            }
        }
        String::from_utf8_lossy(&response).into_owned()
    }

    fn login(proxy: &TestProxy, password: &str) -> String {
        login_from(proxy, password, None)
    }

    fn login_from(proxy: &TestProxy, password: &str, forwarded_ip: Option<&str>) -> String {
        let body = format!("password={password}");
        let forwarded = forwarded_ip
            .map(|ip| format!("CF-Connecting-IP: {ip}\r\n"))
            .unwrap_or_default();
        request(
            proxy,
            &format!(
                "POST /__dsh-remote/login HTTP/1.1\r\nHost: x\r\n{forwarded}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn session_cookie_from(response: &str) -> String {
        response
            .lines()
            .find_map(|line| {
                line.strip_prefix("set-cookie: dsh_remote_lan=")
                    .map(str::to_owned)
            })
            .map(|value| value.split(';').next().unwrap_or_default().to_owned())
            .expect("session cookie issued")
    }

    #[test]
    fn unauthenticated_requests_redirect_to_login() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let response = request(
            &proxy,
            "GET / HTTP/1.1\r\nHost: phone\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 302"), "{response}");
        assert!(
            response.contains("location: /__dsh-remote/login"),
            "{response}"
        );
        assert!(
            seen.lock().expect("seen").is_empty(),
            "nothing may reach upstream"
        );
        let api = request(
            &proxy,
            "POST /api HTTP/1.1\r\nHost: phone\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(api.starts_with("HTTP/1.1 401"), "{api}");
    }

    #[test]
    fn login_page_serves_both_languages() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let zh = request(
            &proxy,
            "GET /__dsh-remote/login HTTP/1.1\r\nHost: x\r\nAccept-Language: zh-CN\r\nConnection: close\r\n\r\n",
        );
        assert!(zh.contains("<h1>DSH Launcher</h1>"), "{zh}");
        assert!(zh.contains("<p class=\"subtitle\">远程访问DSH</p>"), "{zh}");
        assert!(zh.contains("src=\"/__dsh-remote/icon.png\""), "{zh}");
        let en = request(
            &proxy,
            "GET /__dsh-remote/login HTTP/1.1\r\nHost: x\r\nAccept-Language: en-US\r\nConnection: close\r\n\r\n",
        );
        assert!(en.contains("<h1>DSH Launcher</h1>"), "{en}");
        assert!(
            en.contains("<p class=\"subtitle\">Remote access to DSH</p>"),
            "{en}"
        );
    }

    #[test]
    fn login_icon_is_available_without_a_session() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let response = request(
            &proxy,
            "GET /__dsh-remote/icon.png HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("Content-Type: image/png"), "{response}");
        assert!(
            response.contains(&format!("Content-Length: {}", APP_ICON.len())),
            "{response}"
        );
        assert!(seen.lock().expect("seen").is_empty());
    }

    #[test]
    fn correct_password_issues_a_session_that_proxies() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let response = login(&proxy, "12345678");
        assert!(response.starts_with("HTTP/1.1 302"), "{response}");
        assert!(response.contains("location: /"), "{response}");
        let token = session_cookie_from(&response);
        let proxied = request(
            &proxy,
            &format!(
                "GET /chat HTTP/1.1\r\nHost: phone.lan\r\nCookie: upstream_theme=dark; dsh_remote_lan={token}\r\nOrigin: http://192.168.1.9:56740\r\nReferer: http://192.168.1.9:56740/\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(proxied.ends_with("proxied"), "{proxied}");
        let forwarded = seen
            .lock()
            .expect("seen")
            .pop()
            .expect("request reached upstream");
        assert!(forwarded.starts_with("GET /chat HTTP/1.1"), "{forwarded}");
        assert!(forwarded.contains("\r\nHost: 127.0.0.1:"), "{forwarded}");
        assert!(
            !forwarded.contains("dsh_remote_lan"),
            "session token must not leak upstream: {forwarded}"
        );
        assert!(
            forwarded.contains("Cookie: upstream_theme=dark"),
            "Harness-owned cookies must survive the auth rewrite: {forwarded}"
        );
        assert!(forwarded.contains("Connection: close"), "{forwarded}");
        assert!(
            forwarded.contains("\r\nOrigin: http://127.0.0.1:"),
            "origin must be rewritten to the loopback upstream: {forwarded}"
        );
        assert!(
            forwarded.contains("\r\nReferer: http://127.0.0.1:"),
            "referer must be rewritten to the loopback upstream: {forwarded}"
        );
        assert!(
            !forwarded.contains("192.168"),
            "LAN origin must not leak upstream: {forwarded}"
        );
        assert!(
            !forwarded.contains("accept-encoding"),
            "accept-encoding must be stripped: {forwarded}"
        );
    }

    #[test]
    fn authenticated_remote_session_privately_bootstraps_new_harness_auth() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let upstream = FakeUpstream::spawn(Arc::clone(&seen));
        let authority = upstream_config(
            upstream.authority(),
            Some("/?token=launch-secret&mode=web"),
            7,
        );
        let auth = AuthState::new(RemoteScope::Lan, "12345678".to_owned());
        let server = ProxyServer::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Arc::clone(&auth),
            authority,
        )
        .expect("proxy binds");
        let proxy = TestProxy {
            server,
            auth,
            _upstream: upstream,
        };
        let token = session_cookie_from(&login(&proxy, "12345678"));

        let first = request(
            &proxy,
            &format!(
                "GET / HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(first.starts_with("HTTP/1.1 302"), "{first}");
        assert!(
            first.contains("location: /__dsh-remote/bootstrap"),
            "{first}"
        );
        assert!(!first.contains("launch-secret"), "{first}");
        assert!(seen.lock().expect("seen").is_empty());

        let bootstrap = request(
            &proxy,
            &format!(
                "GET /__dsh-remote/bootstrap HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(bootstrap.ends_with("proxied"), "{bootstrap}");
        assert!(!bootstrap.contains("launch-secret"), "{bootstrap}");
        let forwarded = seen.lock().expect("seen").pop().expect("bootstrap request");
        assert!(
            forwarded.starts_with("GET /?token=launch-secret&mode=web HTTP/1.1"),
            "{forwarded}"
        );

        let after = request(
            &proxy,
            &format!(
                "GET / HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(after.ends_with("proxied"), "{after}");
        let forwarded = seen.lock().expect("seen").pop().expect("ordinary request");
        assert!(forwarded.starts_with("GET / HTTP/1.1"), "{forwarded}");
        assert!(!forwarded.contains("launch-secret"), "{forwarded}");
    }

    #[test]
    fn wrong_passwords_redirect_and_eventually_lock_out() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        for _ in 0..5 {
            let response = login(&proxy, "00000000");
            assert!(
                response.contains("/__dsh-remote/login?error=1"),
                "{response}"
            );
        }
        let locked = login(&proxy, "12345678");
        assert!(locked.starts_with("HTTP/1.1 429"), "{locked}");
    }

    #[test]
    fn websocket_upgrade_pipes_raw_bytes_both_ways() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let token = session_cookie_from(&login(&proxy, "12345678"));
        let mut stream = TcpStream::connect(proxy.server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let upgrade = format!(
            "GET /ws HTTP/1.1\r\nHost: phone\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nCookie: dsh_remote_lan={token}\r\n\r\n"
        );
        stream.write_all(upgrade.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).expect("read");
            head.extend_from_slice(&chunk[..count]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        stream.write_all(b"\x81\x02hi").unwrap();
        let mut echo = [0_u8; 4];
        stream.read_exact(&mut echo).expect("echo");
        assert_eq!(&echo, b"\x81\x02hi");
        // The upgrade head must keep its upgrade semantics for upstream.
        let forwarded = seen
            .lock()
            .expect("seen")
            .pop()
            .expect("upgrade reached upstream");
        assert!(forwarded.contains("upgrade: websocket"), "{forwarded}");
        assert!(forwarded.contains("connection: Upgrade"), "{forwarded}");
    }

    #[test]
    fn missing_upstream_yields_503() {
        let auth = AuthState::new(RemoteScope::Lan, "12345678".to_owned());
        let token = auth.sessions.lock().expect("sessions").create();
        let upstream = Arc::new(RwLock::new(None));
        let server = ProxyServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST), auth, upstream).unwrap();
        let proxy = TestProxy {
            server,
            auth: AuthState::new(RemoteScope::Lan, "unused".into()),
            _upstream: FakeUpstream::spawn(Arc::new(Mutex::new(Vec::new()))),
        };
        let response = request(
            &proxy,
            &format!(
                "GET / HTTP/1.1\r\nHost: x\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    }

    #[test]
    fn password_rotation_revokes_sessions_immediately() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let token = session_cookie_from(&login(&proxy, "12345678"));
        proxy.auth.sessions.lock().expect("sessions").clear();
        *proxy.auth.password.write().expect("password") = auth::generate_password();
        let response = request(
            &proxy,
            &format!(
                "GET / HTTP/1.1\r\nHost: x\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(response.starts_with("HTTP/1.1 302"), "{response}");
    }

    /// Regression: clearing the session map alone left already-established
    /// connections (the phone's WebSocket) proxied forever. Revocation must
    /// also shut down every live connection of the scope.
    #[test]
    fn revocation_disconnects_live_connections() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        let token = session_cookie_from(&login(&proxy, "12345678"));

        // Establish a long-lived upgraded (WebSocket) connection.
        let mut stream = TcpStream::connect(proxy.server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let upgrade = format!(
            "GET /ws HTTP/1.1\r\nHost: phone\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nCookie: dsh_remote_lan={token}\r\n\r\n"
        );
        stream.write_all(upgrade.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).expect("read");
            head.extend_from_slice(&chunk[..count]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"), "{head:?}");
        stream.write_all(b"\x81\x02hi").unwrap();
        let mut echo = [0_u8; 4];
        stream
            .read_exact(&mut echo)
            .expect("echo before revocation");
        assert_eq!(&echo, b"\x81\x02hi");

        // Password change: sessions revoked AND live connections dropped.
        *proxy.auth.password.write().expect("password") = auth::generate_password();
        proxy.auth.revoke_all_connections();

        assert!(
            !proxy
                .auth
                .sessions
                .lock()
                .expect("sessions")
                .validate(&token),
            "session revoked"
        );
        let mut buf = [0_u8; 16];
        let result = stream.read(&mut buf);
        assert!(
            matches!(result, Ok(0) | Err(_)),
            "the live connection must be terminated: {result:?}"
        );
    }

    #[test]
    fn login_rate_limit_state_is_scoped_per_ip() {
        // Covered at the limiter level in auth::tests; here the lock must not
        // poison other clients of the same server.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        for _ in 0..5 {
            login(&proxy, "00000000");
        }
        let locked = login(&proxy, "12345678");
        assert!(locked.starts_with("HTTP/1.1 429"), "{locked}");
        let _ = Instant::now();
    }

    #[test]
    fn public_login_rate_limit_uses_cloudflare_visitor_ip() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy_for(RemoteScope::Public, "12345678", &seen);
        for _ in 0..5 {
            login_from(&proxy, "00000000", Some("203.0.113.10"));
        }
        let locked = login_from(&proxy, "12345678", Some("203.0.113.10"));
        assert!(locked.starts_with("HTTP/1.1 429"), "{locked}");

        let other_visitor = login_from(&proxy, "12345678", Some("203.0.113.11"));
        assert!(other_visitor.starts_with("HTTP/1.1 302"), "{other_visitor}");
    }

    #[test]
    fn lan_login_rate_limit_ignores_forwarded_ip_headers() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let proxy = start_proxy("12345678", &seen);
        for index in 1..=5 {
            login_from(&proxy, "00000000", Some(&format!("203.0.113.{index}")));
        }
        let locked = login_from(&proxy, "12345678", Some("203.0.113.200"));
        assert!(locked.starts_with("HTTP/1.1 429"), "{locked}");
    }

    /// A scripted one-shot upstream: reads one request head, replies with
    /// the given raw response bytes, and closes.
    fn scripted_upstream(response: &'static [u8]) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                }
            }
            let _ = stream.write_all(response);
        });
        addr.to_string()
    }

    fn start_proxy_to(authority: String) -> (TestProxy, String) {
        let upstream = upstream_config(authority, None, 1);
        let auth = AuthState::new(RemoteScope::Lan, "12345678".to_owned());
        let token = auth.sessions.lock().expect("sessions").create();
        let server =
            ProxyServer::bind(IpAddr::V4(Ipv4Addr::LOCALHOST), Arc::clone(&auth), upstream)
                .expect("proxy binds");
        let proxy = TestProxy {
            server,
            auth,
            _upstream: FakeUpstream::spawn(Arc::new(Mutex::new(Vec::new()))),
        };
        (proxy, token)
    }

    #[test]
    fn chunked_html_is_dechunked_and_injected_with_polyfills() {
        let page =
            "<!doctype html><html><head><meta charset=\"utf-8\"></head><body>hi</body></html>";
        let (left, right) = page.split_at(20);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            left.len(),
            left,
            right.len(),
            right
        );
        let leaked: &'static [u8] = Box::leak(response.into_bytes().into_boxed_slice());
        let (proxy, token) = start_proxy_to(scripted_upstream(leaked));
        let proxied = request(
            &proxy,
            &format!(
                "GET / HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(proxied.starts_with("HTTP/1.1 200"), "{proxied}");
        assert!(!proxied.contains("Transfer-Encoding"), "{proxied}");
        let polyfill = proxied
            .find("data-dsh-remote-polyfill=\"1\"")
            .expect("api polyfill injected");
        let head_open = proxied.find("<head>").expect("head tag");
        assert!(
            polyfill > head_open,
            "polyfill follows the head tag: {proxied}"
        );
        assert!(
            proxied.contains("<head><script data-dsh-remote-polyfill=\"1\">"),
            "polyfill is injected immediately after <head>: {proxied}"
        );
        assert!(proxied.contains("randomUUID=function"), "{proxied}");
        assert!(proxied.contains("AbortSignal.any"), "{proxied}");
        assert!(
            proxied.contains("</body></html>"),
            "page survives intact: {proxied}"
        );
        // The rewritten head must advertise the new exact body length.
        let declared: usize = proxied
            .split("Content-Length: ")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .and_then(|value| value.parse().ok())
            .expect("content-length present");
        let body = proxied.split("\r\n\r\n").nth(1).expect("body");
        assert_eq!(declared, body.len(), "length matches the injected body");
    }

    #[test]
    fn non_html_chunked_responses_stream_through_with_framing_intact() {
        let response: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let (proxy, token) = start_proxy_to(scripted_upstream(response));
        let proxied = request(
            &proxy,
            &format!(
                "GET /data HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            proxied.ends_with("3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"),
            "chunk framing must pass through untouched: {proxied}"
        );
        assert!(
            !proxied.contains("data-dsh-remote"),
            "no injection into text/plain"
        );
    }

    #[test]
    fn head_response_does_not_wait_for_or_inject_the_declared_body() {
        // A HEAD Content-Length describes the GET representation. There is
        // deliberately no body for the proxy to read.
        let response: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n";
        let (proxy, token) = start_proxy_to(scripted_upstream(response));
        let proxied = request(
            &proxy,
            &format!(
                "HEAD / HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(proxied.starts_with("HTTP/1.1 200"), "{proxied}");
        assert!(proxied.contains("Content-Length: 4096"), "{proxied}");
        assert!(!proxied.contains("data-dsh-remote-polyfill"), "{proxied}");
        assert!(
            proxied.ends_with("\r\n\r\n"),
            "HEAD must not gain a body: {proxied}"
        );
    }

    #[test]
    fn keep_alive_connections_carry_multiple_responses() {
        // Two responses on one upstream connection, both content-length framed.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            for body in [b"first".as_slice(), b"second".as_slice()] {
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).is_err() || stream.write_all(body).is_err()
                {
                    return;
                }
            }
        });
        let (proxy, token) = start_proxy_to(addr.to_string());
        let mut stream = TcpStream::connect(proxy.server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw = Vec::new();
        for expected in ["first", "second"] {
            stream
                .write_all(
                    format!(
                        "GET /x HTTP/1.1\r\nHost: phone\r\nCookie: dsh_remote_lan={token}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            let mut chunk = [0_u8; 4096];
            // Read until the head plus exactly the declared body length.
            loop {
                let count = stream.read(&mut chunk).expect("read");
                raw.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want: usize = head
                        .split("Content-Length: ")
                        .nth(1)
                        .and_then(|rest| rest.split("\r\n").next())
                        .and_then(|value| value.parse().ok())
                        .expect("content-length");
                    if body.len() >= want {
                        assert!(body.starts_with(expected), "{body}");
                        raw = body.as_bytes()[want..].to_vec();
                        break;
                    }
                }
            }
        }
    }
}
