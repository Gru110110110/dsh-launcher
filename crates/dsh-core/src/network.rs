//! Unified proxy configuration and HTTP client construction for every
//! Launcher-owned network path.
//!
//! All networking in `dsh-core` (Harness version queries, packuments,
//! tarballs, Node downloads, release source checks, marketplace catalog,
//! registry, and GitHub clients) and every networked subprocess (npm, pnpm,
//! the Harness CLI) resolves its proxy behavior through this module so the
//! three mutually exclusive modes — system, direct, manual — behave the same
//! everywhere. The Tauri adapter only mirrors the updater builder and IPC;
//! it holds no proxy logic of its own.
//!
//! Security notes:
//! - Manual proxy URLs carrying userinfo are rejected at validation time, so
//!   no credential can be persisted, logged, or surface in an error message.
//! - Error details are sanitized defensively anyway: any `scheme://userinfo@`
//!   fragment is redacted before a diagnostic leaves this module.
//! - The Windows registry integration is strictly read-only and limited to
//!   the current user's `Internet Settings` key; it never writes system or
//!   proxy state.

use std::{
    ffi::OsString,
    process::Command,
    sync::{OnceLock, RwLock},
};

use regex::Regex;
use reqwest::blocking::{Client, ClientBuilder};
use url::Url;

use crate::{
    AppError, AppResult,
    model::{NetworkErrorKind, ProxyMode, ProxySettings},
};

/// Proxy URL schemes supported in manual mode.
const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

/// Proxy-related environment variables (both casings) that subprocess
/// environments must control deterministically instead of vaguely inheriting.
pub const PROXY_ENV_KEYS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Unit-test opt-in for exercising proxy environment handling. Production
/// code never reads this variable. Without the marker, test clients and
/// Launcher-owned test subprocesses are forced direct so they cannot discover
/// or contact a developer's real OS proxy; behavior tests set it only inside
/// an env-cleared test process.
#[cfg(test)]
const TEST_SYSTEM_PROXY_ENV: &str = "DSH_DESKTOP_TEST_SYSTEM_PROXY";

/// Maximum length of a sanitized diagnostic detail.
const MAX_DETAIL_LEN: usize = 300;

/// Validates a proxy configuration. Only manual mode carries input that can
/// be malformed; system and direct are always valid.
pub fn validate(settings: &ProxySettings) -> AppResult<()> {
    if settings.mode == ProxyMode::Manual {
        manual_url(settings)?;
    }
    Ok(())
}

/// Produces the canonical representation allowed to cross the persistence
/// boundary. Manual values are validated and trimmed. Inactive URL and bypass
/// fields are erased for system/direct modes, so a hidden stale value (most
/// importantly URL userinfo) can never be written to preferences or exposed
/// through the launcher snapshot.
pub fn for_persistence(mut settings: ProxySettings) -> AppResult<ProxySettings> {
    settings.url = settings.url.trim().to_owned();
    settings.bypass = settings.bypass.trim().to_owned();
    if settings.mode == ProxyMode::Manual {
        validate(&settings)?;
    } else {
        settings.url.clear();
        settings.bypass.clear();
    }
    Ok(settings)
}

/// Parses and strictly validates the manual proxy URL. The error `reason`
/// value is a stable, localizable token and never echoes the raw URL, so a
/// rejected credential can never leak through diagnostics.
fn manual_url(settings: &ProxySettings) -> AppResult<Url> {
    let raw = settings.url.trim();
    let invalid = |reason: &str| AppError::new("proxyUrlInvalid").value("reason", reason);
    if raw.is_empty() {
        return Err(invalid("missing"));
    }
    let url = Url::parse(raw).map_err(|_| invalid("invalid"))?;
    if !PROXY_SCHEMES.contains(&url.scheme()) {
        return Err(invalid("scheme"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        // Proxy credentials are out of scope for this version and are never
        // persisted anywhere; reject them instead of silently dropping them.
        return Err(invalid("credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        // A proxy endpoint is scheme://host[:port] only; anything else is a
        // malformed configuration, not a proxy with extras.
        return Err(invalid("path"));
    }
    Ok(url)
}

/// Normalizes one bypass entry into the reqwest/NO_PROXY domain grammar.
///
/// reqwest's `NoProxy` (and curl/Go/npm-style matchers) understand `*` and
/// plain or leading-dot domain rules, and IPv4/IPv6 CIDR ranges, but not the
/// IE-style `*.domain` wildcard; the latter is rewritten to the equivalent
/// leading-dot domain rule. Entries that are not remotely a host/domain/CIDR
/// (schemes, userinfo, paths, arbitrary wildcards, whitespace) are dropped so
/// they can never corrupt the list — Windows-only wildcard forms are never
/// passed through to reqwest.
pub fn normalize_bypass_entry(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if entry == "*" {
        return Some(entry.to_owned());
    }
    // reqwest's NoProxy supports "ips, possibly with subnet mask" — keep
    // syntactically valid CIDR ranges instead of dropping them.
    if entry.contains('/') {
        let valid = entry.split_once('/').is_some_and(|(address, prefix)| {
            address.parse::<std::net::IpAddr>().is_ok_and(|ip| {
                let bits: u8 = if ip.is_ipv4() { 32 } else { 128 };
                prefix.parse::<u8>().is_ok_and(|length| length <= bits)
            })
        });
        return valid.then(|| entry.to_owned());
    }
    let normalized = match entry.strip_prefix("*.") {
        Some(rest) => format!(".{rest}"),
        None => entry.to_owned(),
    };
    let valid = normalized != "."
        && !normalized.is_empty()
        && normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
    valid.then_some(normalized)
}

/// Normalizes a user-provided bypass list to the comma-separated NO_PROXY
/// shape. Accepts commas and semicolons as separators.
pub fn normalized_bypass(raw: &str) -> Option<String> {
    let entries: Vec<_> = raw
        .split([',', ';'])
        .filter_map(normalize_bypass_entry)
        .collect();
    (!entries.is_empty()).then(|| entries.join(","))
}

/// The resolved per-protocol system proxy configuration.
///
/// Priority per protocol: the matching environment variable (first valid
/// value across casings), then `ALL_PROXY`/`all_proxy`, then — only for
/// protocols still unresolved — the corresponding Windows Internet Settings
/// entry (read-only, injectable). `NO_PROXY`/`no_proxy` likewise wins over
/// `ProxyOverride`. In a CGI environment (`REQUEST_METHOD` present) no proxy
/// is resolved at all.
///
/// This plan drives two things: the explicit takeover of reqwest's proxy
/// handling on Windows (see [`takeover_system_plan`], needed because
/// hyper-util copies a raw per-protocol `ProxyServer` string verbatim and
/// ignores its `socks` entry), and the proxy variables injected into
/// subprocesses. On macOS/Linux the core and updater clients instead keep
/// reqwest's default env+OS-proxy merging, and the plan is env-only there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemProxyPlan {
    /// Resolved proxy for plain-HTTP targets (may come from `ALL_PROXY`).
    pub http: Option<String>,
    /// Resolved proxy for HTTPS targets (may come from `ALL_PROXY`).
    pub https: Option<String>,
    /// A genuine all-protocol/SOCKS source (env `ALL_PROXY` or WinInet
    /// `socks=`), forwarded to subprocesses as `ALL_PROXY`.
    pub all: Option<String>,
    pub no_proxy: Option<String>,
}

impl SystemProxyPlan {
    /// True when no proxy source resolved. A lone NO_PROXY does not make a
    /// plan non-empty: without any proxy there is nothing to bypass.
    pub fn has_proxies(&self) -> bool {
        self.http.is_some() || self.https.is_some() || self.all.is_some()
    }
}

/// Valid proxy URL schemes accepted from environment variables (a superset of
/// the manual-mode schemes: env proxies may legitimately use socks4(a)).
const ENV_PROXY_SCHEMES: [&str; 6] = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// Normalizes an environment proxy value. Mirrors reqwest/hyper-util
/// compatibility: a scheme-less traditional `host[:port]` is treated as an
/// HTTP proxy; explicit schemes must be proxy schemes with a non-empty host.
/// Anything else is garbage and never forwarded.
fn valid_env_proxy(raw: String) -> Option<String> {
    let raw = raw.trim();
    let candidate = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let url = Url::parse(&candidate).ok()?;
    (ENV_PROXY_SCHEMES.contains(&url.scheme()) && url.host_str().is_some_and(|h| !h.is_empty()))
        .then_some(candidate)
}

/// Resolves the effective system proxy plan from the environment and an
/// injectable Windows proxy source. Pure: tests pass fakes, production passes
/// the real environment and (on Windows) the read-only registry reader.
///
/// CGI safety (matching reqwest/hyper-util): when `REQUEST_METHOD` is present
/// the process serves an HTTP request and environment proxy variables must
/// never be trusted, so no proxy is resolved or forwarded at all.
pub fn system_proxy_plan(
    lookup: &dyn Fn(&str) -> Option<OsString>,
    windows_source: Option<&dyn WindowsProxySource>,
) -> SystemProxyPlan {
    if lookup("REQUEST_METHOD").is_some() {
        return SystemProxyPlan::default();
    }
    // First valid, normalized value wins across casings: an invalid or empty
    // HTTP_PROXY must not shadow a valid http_proxy.
    let env = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .filter_map(|key| lookup(key))
            .filter_map(|value| value.into_string().ok())
            .find_map(valid_env_proxy)
    };
    let env_http = env(&["HTTP_PROXY", "http_proxy"]);
    let env_https = env(&["HTTPS_PROXY", "https_proxy"]);
    let env_all = env(&["ALL_PROXY", "all_proxy"]);
    let env_no_proxy = ["NO_PROXY", "no_proxy"]
        .iter()
        .filter_map(|key| lookup(key))
        .filter_map(|value| value.into_string().ok())
        .find_map(|value| normalized_bypass(&value));

    let wininet = windows_source
        .and_then(|source| source.read())
        .filter(|proxy| proxy.enable);
    let server = wininet
        .as_ref()
        .map(|proxy| parse_wininet_server(&proxy.server))
        .unwrap_or_default();
    let wininet_no_proxy = wininet
        .as_ref()
        .map(|proxy| wininet_bypass_list(&proxy.override_list))
        .filter(|list| !list.is_empty())
        .map(|list| list.join(","));

    // Per protocol: env variable → env ALL_PROXY → WinInet per-protocol entry
    // → WinInet socks (the registry's all-protocol analogue). `all` itself is
    // only a genuine all-protocol source, forwarded to subprocesses as-is.
    let all = env_all.clone().or_else(|| server.socks.clone());
    SystemProxyPlan {
        http: env_http
            .or(env_all.clone())
            .or(server.http)
            .or_else(|| server.socks.clone()),
        https: env_https.or(env_all).or(server.https).or(server.socks),
        all,
        no_proxy: env_no_proxy.or(wininet_no_proxy),
    }
}

/// Decides whether the client takes over proxy handling with an explicit
/// merged per-protocol plan, or keeps reqwest's default env/system merging.
///
/// Explicit takeover exists solely to bypass hyper-util's broken per-protocol
/// Windows `ProxyServer` matcher, so it applies only when a Windows proxy
/// source is present (`platform_proxy_source()` returns `None` elsewhere).
/// On macOS/Linux the default builder already merges environment variables
/// with the OS system proxy — forcing an env-only explicit plan there would
/// wrongly disable the macOS system proxy as soon as any single proxy
/// variable is set. Pure and injectable: the decision depends only on the
/// source, and the returned plan is resolved with the same pure resolver.
pub fn takeover_system_plan(
    lookup: &dyn Fn(&str) -> Option<OsString>,
    windows_source: Option<&dyn WindowsProxySource>,
) -> Option<SystemProxyPlan> {
    let source = windows_source?;
    let plan = system_proxy_plan(lookup, Some(source));
    plan.has_proxies().then_some(plan)
}

/// Applies a resolved system plan to a blocking reqwest 0.12 builder. Only
/// called on the Windows takeover path (see [`takeover_system_plan`]); it
/// disables reqwest's own env/system reading and installs the merged
/// per-protocol proxies.
pub fn apply_system_plan(
    builder: ClientBuilder,
    plan: &SystemProxyPlan,
) -> AppResult<ClientBuilder> {
    let no_proxy = plan
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);
    let invalid = |error: reqwest::Error| {
        AppError::new("networkClientFailed").detail(sanitize_detail(&error.to_string()))
    };
    let mut builder = builder.no_proxy();
    if let Some(http) = &plan.http {
        builder = builder.proxy(
            reqwest::Proxy::http(http)
                .map_err(invalid)?
                .no_proxy(no_proxy.clone()),
        );
    }
    if let Some(https) = &plan.https {
        builder = builder.proxy(
            reqwest::Proxy::https(https)
                .map_err(invalid)?
                .no_proxy(no_proxy.clone()),
        );
    }
    Ok(builder)
}

/// Builds a blocking reqwest client builder preconfigured for the given proxy
/// settings. Callers add their own timeouts and build the client.
///
/// - system: on Windows, the merged per-protocol plan from environment
///   variables and the read-only Internet Settings fallback (see
///   [`takeover_system_plan`]); elsewhere — and whenever nothing explicit
///   resolved — reqwest's default behavior, which merges proxy environment
///   variables with the OS system proxy itself.
/// - direct: `no_proxy`, ignoring system proxy settings and every proxy
///   environment variable.
/// - manual: one explicit `Proxy` for all protocols plus the bypass list.
pub fn blocking_builder(user_agent: &str, settings: &ProxySettings) -> AppResult<ClientBuilder> {
    let builder = Client::builder().user_agent(user_agent.to_owned());
    // Native roots on macOS are loaded through the User/Admin/System Trust
    // Settings APIs (the system Keychain trust domains). Tests must never read
    // those real stores, so unit-test clients keep the bundled WebPKI roots
    // only. Production still combines WebPKI and native roots.
    #[cfg(test)]
    let builder = builder.tls_built_in_native_certs(false);
    match settings.mode {
        ProxyMode::System => {
            #[cfg(test)]
            {
                // Default unit-test behavior is direct: never inspect the real
                // macOS dynamic store or Windows Internet Settings and never
                // inherit the host runner's proxy environment. The dedicated
                // env-proxy behavior children opt in with an env-cleared,
                // controlled process and still never receive an OS source.
                if std::env::var_os(TEST_SYSTEM_PROXY_ENV).is_none() {
                    return Ok(builder.no_proxy());
                }
                let plan = system_proxy_plan(&|key| std::env::var_os(key), None);
                if plan.has_proxies() {
                    apply_system_plan(builder, &plan)
                } else {
                    Ok(builder.no_proxy())
                }
            }
            #[cfg(not(test))]
            {
                match takeover_system_plan(&|key| std::env::var_os(key), platform_proxy_source()) {
                    Some(plan) => apply_system_plan(builder, &plan),
                    None => Ok(builder),
                }
            }
        }
        ProxyMode::Direct => Ok(builder.no_proxy()),
        ProxyMode::Manual => {
            let url = manual_url(settings)?;
            let proxy = reqwest::Proxy::all(url.as_str())
                .map_err(|_| AppError::new("proxyUrlInvalid").value("reason", "invalid"))?;
            let proxy = match normalized_bypass(&settings.bypass)
                .and_then(|list| reqwest::NoProxy::from_string(&list))
            {
                Some(no_proxy) => proxy.no_proxy(Some(no_proxy)),
                None => proxy,
            };
            Ok(builder.proxy(proxy))
        }
    }
}

/// Builds a blocking client for explicit settings.
pub fn blocking_client(user_agent: &str, settings: &ProxySettings) -> AppResult<Client> {
    blocking_builder(user_agent, settings)?
        .build()
        .map_err(|error| {
            AppError::new("networkClientFailed").detail(sanitize_detail(&error.to_string()))
        })
}

fn active_cell() -> &'static RwLock<ProxySettings> {
    static CELL: OnceLock<RwLock<ProxySettings>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(ProxySettings::default()))
}

/// Makes the given settings effective for the next network operation and the
/// next subprocess. Called by the adapter after loading or atomically saving
/// preferences, so a saved proxy change never requires a restart.
pub fn activate(settings: ProxySettings) {
    *active_cell().write().expect("proxy settings poisoned") = settings;
}

/// The currently active proxy settings. Defaults to `system`, matching the
/// behavior of configurations written before proxy support existed.
pub fn active() -> ProxySettings {
    active_cell()
        .read()
        .expect("proxy settings poisoned")
        .clone()
}

/// Builds a blocking client from the active settings.
pub fn active_blocking_client(user_agent: &str) -> AppResult<Client> {
    blocking_client(user_agent, &active())
}

/// Redacts anything that looks like URL userinfo (`scheme://userinfo@`),
/// drops reqwest's embedded `for url (...)` clause (callers already label
/// their source, so the embedded URL only repeats it), and bounds the length,
/// so diagnostics derived from transport errors can neither leak proxy
/// credentials inherited from the environment nor duplicate addresses.
pub fn sanitize_detail(raw: &str) -> String {
    static USERINFO: OnceLock<Regex> = OnceLock::new();
    static REQUEST_URL: OnceLock<Regex> = OnceLock::new();
    let userinfo = USERINFO.get_or_init(|| {
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]+@").expect("userinfo pattern")
    });
    let request_url = REQUEST_URL
        .get_or_init(|| Regex::new(r"\s*for url \([^)]*\)").expect("request url pattern"));
    let stripped = request_url.replace_all(raw, "");
    let redacted = userinfo.replace_all(&stripped, "${1}***@");
    let mut detail = redacted.chars().take(MAX_DETAIL_LEN).collect::<String>();
    if redacted.chars().count() > MAX_DETAIL_LEN {
        detail.push('…');
    }
    detail
}

/// The classified, sanitized outcome of a failed reqwest operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedNetworkError {
    pub kind: NetworkErrorKind,
    pub detail: String,
}

/// Conservatively classifies a reqwest error. Only clearly recognizable
/// failures get a specific kind; everything else stays `Other`.
pub fn classify_reqwest(error: &reqwest::Error) -> ClassifiedNetworkError {
    let kind = if error.is_timeout() {
        NetworkErrorKind::Timeout
    } else if error.status() == Some(reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED) {
        NetworkErrorKind::ProxyAuth
    } else if chain_mentions_proxy_auth(error) {
        // A 407 answering an HTTPS CONNECT tunnel is a request error without
        // a status; the source chain carries an explicit proxy-auth message.
        NetworkErrorKind::ProxyAuth
    } else if chain_mentions_tls(error) {
        NetworkErrorKind::Tls
    } else if error.is_connect() {
        // reqwest reports DNS resolution failures as connect errors, so this
        // covers refused connections, unreachable hosts, and DNS alike.
        NetworkErrorKind::Connect
    } else if error.status().is_some() {
        NetworkErrorKind::HttpStatus
    } else {
        NetworkErrorKind::Other
    };
    ClassifiedNetworkError {
        kind,
        detail: sanitize_detail(&error.to_string()),
    }
}

fn chain_mentions_proxy_auth(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        let text = error.to_string().to_ascii_lowercase();
        if text.contains("proxy authorization required")
            || text.contains("proxy authentication required")
        {
            return true;
        }
        source = error.source();
    }
    false
}

fn chain_mentions_tls(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        let text = error.to_string().to_ascii_lowercase();
        if text.contains("certificate") || text.contains("tls") {
            return true;
        }
        source = error.source();
    }
    false
}

/// A read-only snapshot of the current user's Windows Internet Settings proxy
/// values (`ProxyEnable`, `ProxyServer`, `ProxyOverride`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WinInetProxy {
    pub enable: bool,
    pub server: String,
    pub override_list: String,
}

/// Read-only access to the Windows per-user proxy configuration. Injecting
/// this trait keeps every registry conversion testable without ever touching
/// a real registry hive.
pub trait WindowsProxySource: Send + Sync {
    fn read(&self) -> Option<WinInetProxy>;
}

/// The platform proxy source: the real registry on Windows, absent elsewhere.
pub fn platform_proxy_source() -> Option<&'static dyn WindowsProxySource> {
    #[cfg(all(windows, not(test)))]
    {
        static SOURCE: RegistryProxySource = RegistryProxySource;
        Some(&SOURCE)
    }
    #[cfg(any(not(windows), test))]
    {
        None
    }
}

/// Computes the exact proxy-related environment pairs for a subprocess.
/// `lookup` reads inherited environment variables; `windows_source` is only
/// consulted in system mode.
///
/// - direct: no pairs — combined with the caller's `env_clear`, this erases
///   every inherited proxy variable in both casings.
/// - manual: injects the proxy URL and bypass list in both casings, including
///   `ALL_PROXY`/`all_proxy`.
/// - system: emits the merged per-protocol plan (see [`system_proxy_plan`]) —
///   environment variables win per protocol, `ALL_PROXY` is the fallback, and
///   Windows Internet Settings only fill protocols the environment did not
///   cover. npm does not read `ALL_PROXY`, so the resolved HTTP/HTTPS values
///   are always emitted per protocol. This rebuilds environment variables
///   only: the macOS/Linux OS system proxy is never exported to subprocesses
///   (there is no variable form for it), and in a CGI environment
///   (`REQUEST_METHOD` set) no proxy variables are emitted at all.
pub fn subprocess_env(
    settings: &ProxySettings,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    windows_source: Option<&dyn WindowsProxySource>,
) -> Vec<(String, OsString)> {
    match settings.mode {
        ProxyMode::Direct => Vec::new(),
        ProxyMode::Manual => {
            let Ok(url) = manual_url(settings) else {
                // Invalid settings are rejected before they can be saved; a
                // racing caller must not inject a malformed proxy variable.
                return Vec::new();
            };
            let mut pairs: Vec<(String, OsString)> = [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
            ]
            .iter()
            .map(|key| ((*key).to_owned(), OsString::from(url.as_str())))
            .collect();
            if let Some(bypass) = normalized_bypass(&settings.bypass) {
                pairs.push(("NO_PROXY".to_owned(), OsString::from(&bypass)));
                pairs.push(("no_proxy".to_owned(), OsString::from(bypass)));
            }
            pairs
        }
        ProxyMode::System => {
            let plan = system_proxy_plan(lookup, windows_source);
            let mut pairs = Vec::new();
            let mut push = |keys: [&str; 2], value: &Option<String>| {
                if let Some(value) = value {
                    pairs.push((keys[0].to_owned(), OsString::from(value)));
                    pairs.push((keys[1].to_owned(), OsString::from(value)));
                }
            };
            push(["HTTP_PROXY", "http_proxy"], &plan.http);
            push(["HTTPS_PROXY", "https_proxy"], &plan.https);
            push(["ALL_PROXY", "all_proxy"], &plan.all);
            push(["NO_PROXY", "no_proxy"], &plan.no_proxy);
            pairs
        }
    }
}

fn command_subprocess_env(
    settings: &ProxySettings,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    windows_source: Option<&dyn WindowsProxySource>,
    allow_system_sources: bool,
) -> Vec<(String, OsString)> {
    if settings.mode == ProxyMode::System && !allow_system_sources {
        return Vec::new();
    }
    subprocess_env(settings, lookup, windows_source)
}

/// Resolves the active proxy environment for a Launcher-owned subprocess.
/// Unit tests default to direct and never expose the real Windows registry;
/// an env-cleared behavior-test child may opt in to controlled environment
/// variables with [`TEST_SYSTEM_PROXY_ENV`].
pub(crate) fn active_subprocess_env() -> Vec<(String, OsString)> {
    let settings = active();
    #[cfg(test)]
    let allow_system_sources = std::env::var_os(TEST_SYSTEM_PROXY_ENV).is_some();
    #[cfg(not(test))]
    let allow_system_sources = true;
    command_subprocess_env(
        &settings,
        &|key| std::env::var_os(key),
        platform_proxy_source(),
        allow_system_sources,
    )
}

/// Applies the active proxy configuration to an already environment-cleared
/// command: removes every proxy variable in both casings, then injects the
/// pairs the unified configuration computes.
pub fn apply_to_command(command: &mut Command) {
    let pairs = active_subprocess_env();
    for key in PROXY_ENV_KEYS {
        command.env_remove(key);
    }
    for (key, value) in pairs {
        command.env(key, value);
    }
}

/// True for proxy-related environment variable names in any casing.
pub fn is_proxy_env_key(key: &str) -> bool {
    PROXY_ENV_KEYS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(key))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WinInetServer {
    http: Option<String>,
    https: Option<String>,
    socks: Option<String>,
}

/// Parses a Windows `ProxyServer` value. Pure function: never touches the
/// registry itself.
///
/// `ProxyServer` accepts either a single `host:port` address used for every
/// protocol or a per-protocol `http=...;https=...;socks=...` list.
fn parse_wininet_server(raw: &str) -> WinInetServer {
    let raw = raw.trim();
    if raw.is_empty() {
        return WinInetServer::default();
    }
    let with_scheme = |scheme: &str, value: &str| -> Option<String> {
        let value = value.trim().trim_end_matches('/');
        if value.is_empty() {
            return None;
        }
        if value.contains("://") {
            let parsed = Url::parse(value).ok()?;
            if !PROXY_SCHEMES.contains(&parsed.scheme())
                || parsed.host_str().is_none_or(str::is_empty)
            {
                return None;
            }
            Some(parsed.to_string().trim_end_matches('/').to_owned())
        } else {
            let candidate = format!("{scheme}://{value}");
            let parsed = Url::parse(&candidate).ok()?;
            if parsed.host_str().is_none_or(str::is_empty) {
                return None;
            }
            Some(candidate)
        }
    };
    if raw.contains('=') {
        let mut server = WinInetServer::default();
        for part in raw.split(';') {
            let Some((scheme, value)) = part.split_once('=') else {
                continue;
            };
            match scheme.trim().to_ascii_lowercase().as_str() {
                "http" => server.http = with_scheme("http", value),
                "https" => server.https = with_scheme("http", value),
                "socks" | "socks5" => server.socks = with_scheme("socks5", value),
                _ => {}
            }
        }
        server
    } else {
        // Single-address form: one HTTP proxy serves every protocol.
        let single = with_scheme("http", raw);
        WinInetServer {
            http: single.clone(),
            https: single,
            socks: None,
        }
    }
}

/// Converts a `ProxyOverride` list into NO_PROXY entries. The `<local>`
/// token ("bypass for local addresses") becomes explicit loopback entries so
/// npm/pnpm, which do not understand the token, still bypass local traffic.
/// Other entries go through the same normalization as the manual bypass list,
/// so the reqwest and subprocess paths agree.
pub fn wininet_bypass_list(raw: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.eq_ignore_ascii_case("<local>") {
            for loopback in ["localhost", "127.0.0.1", "::1"] {
                if !entries.iter().any(|entry| entry == loopback) {
                    entries.push(loopback.to_owned());
                }
            }
        } else if let Some(normalized) = normalize_bypass_entry(part)
            && !entries.contains(&normalized)
        {
            entries.push(normalized);
        }
    }
    entries
}

/// Read-only registry access to `HKCU\...\Internet Settings`. Compiled only
/// for non-test Windows builds; tests everywhere exercise the pure conversion
/// functions with fakes instead of compiling or opening the real hive.
#[cfg(all(windows, not(test)))]
struct RegistryProxySource;

#[cfg(all(windows, not(test)))]
impl WindowsProxySource for RegistryProxySource {
    fn read(&self) -> Option<WinInetProxy> {
        read_wininet_registry()
    }
}

#[cfg(all(windows, not(test)))]
fn read_wininet_registry() -> Option<WinInetProxy> {
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
            RegOpenKeyExW, RegQueryValueExW,
        },
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe {
        let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
        let mut key: HKEY = ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
            return None;
        }
        let result = (|| {
            let enable = {
                let name = wide("ProxyEnable");
                let mut value: u32 = 0;
                let mut kind: u32 = 0;
                let mut size = size_of::<u32>() as u32;
                let status = RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    ptr::null(),
                    &mut kind,
                    &mut value as *mut u32 as *mut u8,
                    &mut size,
                );
                (status == ERROR_SUCCESS && kind == REG_DWORD).then(|| value != 0)
            };
            let string = |value_name: &str| -> Option<String> {
                let name = wide(value_name);
                let mut kind: u32 = 0;
                let mut size: u32 = 0;
                let status = RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    ptr::null(),
                    &mut kind,
                    ptr::null_mut(),
                    &mut size,
                );
                if status != ERROR_SUCCESS || !matches!(kind, REG_SZ | REG_EXPAND_SZ) || size == 0 {
                    return None;
                }
                let mut buffer = vec![0_u16; (size as usize / 2) + 1];
                let status = RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    ptr::null(),
                    &mut kind,
                    buffer.as_mut_ptr() as *mut u8,
                    &mut size,
                );
                if status != ERROR_SUCCESS {
                    return None;
                }
                let end = buffer
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(buffer.len());
                Some(String::from_utf16_lossy(&buffer[..end]))
            };
            Some(WinInetProxy {
                enable: enable?,
                server: string("ProxyServer").unwrap_or_default(),
                override_list: string("ProxyOverride").unwrap_or_default(),
            })
        })();
        RegCloseKey(key);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        process::{Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    fn settings(mode: ProxyMode, url: &str, bypass: &str) -> ProxySettings {
        ProxySettings {
            mode,
            url: url.to_owned(),
            bypass: bypass.to_owned(),
        }
    }

    struct FakeSource(Option<WinInetProxy>);

    impl WindowsProxySource for FakeSource {
        fn read(&self) -> Option<WinInetProxy> {
            self.0.clone()
        }
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + use<'a> {
        move |key| {
            pairs
                .iter()
                .find_map(|(name, value)| (*name == key).then(|| OsString::from(value)))
        }
    }

    // -----------------------------------------------------------------------
    // Stoppable loopback test servers. Every server is bound, owned, and
    // stopped by the test that created it: Drop sets the stop flag, wakes the
    // blocking accept with a self-connect, and joins the thread, so no test
    // can leak a listener or a detached thread.
    // -----------------------------------------------------------------------

    struct TestServer {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(handler: impl Fn(TcpStream, &AtomicBool) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    handler(stream, &flag);
                }
            });
            Self {
                address,
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        fn stop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the blocking accept so the server thread can observe the
            // stop flag and exit promptly.
            let _ = TcpStream::connect(self.address);
            if let Some(handle) = self.handle.take() {
                handle.join().expect("test server thread panicked");
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => request.extend_from_slice(&buffer[..count]),
            }
        }
        request
    }

    /// Serves one plain-text response per connection.
    fn serve_text(body: &'static str) -> TestServer {
        TestServer::spawn(move |mut stream, _stop| {
            let _ = read_request_head(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        })
    }

    /// A minimal HTTP proxy: records every request line and answers 200
    /// without forwarding, so tests can prove exactly where traffic went.
    fn fake_http_proxy() -> (TestServer, Arc<Mutex<Vec<String>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let server = TestServer::spawn(move |mut stream, _stop| {
            let request = read_request_head(&mut stream);
            let text = String::from_utf8_lossy(&request).into_owned();
            seen.lock()
                .expect("requests poisoned")
                .push(text.lines().next().unwrap_or_default().to_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nproxied",
            );
        });
        (server, requests)
    }

    fn closed_loopback_url(path: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}{path}")
    }

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    #[test]
    fn manual_mode_accepts_supported_schemes() {
        for scheme in ["http", "https", "socks5", "socks5h"] {
            let candidate = settings(ProxyMode::Manual, &format!("{scheme}://127.0.0.1:7890"), "");
            assert!(validate(&candidate).is_ok(), "scheme {scheme}");
        }
    }

    #[test]
    fn manual_mode_rejects_bad_urls_and_credentials() {
        let rejected = [
            ("", "missing"),
            ("   ", "missing"),
            ("not-a-url", "invalid"),
            ("ftp://127.0.0.1:21", "scheme"),
            ("socks5://", "host"),
            ("http://user@127.0.0.1:8080", "credentials"),
            ("http://user:pass@127.0.0.1:8080", "credentials"),
            ("http://127.0.0.1:8080?x=1", "path"),
            ("http://127.0.0.1:8080#frag", "path"),
            ("http://127.0.0.1:8080/proxy", "path"),
            ("socks5h://127.0.0.1:1080/", "ok"),
        ];
        for (url, reason) in rejected {
            let candidate = settings(ProxyMode::Manual, url, "");
            if reason == "ok" {
                assert!(validate(&candidate).is_ok(), "{url}");
                continue;
            }
            let error = validate(&candidate).expect_err(url);
            assert_eq!(error.code, "proxyUrlInvalid", "{url}");
            assert_eq!(
                error.values.get("reason").map(String::as_str),
                Some(reason),
                "{url}"
            );
            // The raw URL must never be echoed back, so userinfo in a
            // rejected candidate can never leak through the error.
            assert_eq!(error.safe_detail, None);
            let rendered = format!("{error:?}");
            assert!(!rendered.contains("user"), "{rendered}");
        }
    }

    #[test]
    fn system_and_direct_modes_are_always_valid() {
        assert!(validate(&ProxySettings::default()).is_ok());
        assert!(validate(&settings(ProxyMode::Direct, "", "")).is_ok());
        // A stale manual URL must not break direct/system configurations.
        assert!(validate(&settings(ProxyMode::Direct, "not-a-url", "")).is_ok());
        assert!(validate(&settings(ProxyMode::System, "user:pw@x", "")).is_ok());
    }

    #[test]
    fn persistence_clears_inactive_fields_and_rejects_manual_credentials() {
        assert_eq!(
            for_persistence(settings(
                ProxyMode::Direct,
                "http://user:topsecret@proxy.invalid:8080",
                "localhost",
            ))
            .unwrap(),
            settings(ProxyMode::Direct, "", "")
        );
        let error = for_persistence(settings(
            ProxyMode::Manual,
            "http://user:topsecret@proxy.invalid:8080",
            "",
        ))
        .unwrap_err();
        assert_eq!(error.code, "proxyUrlInvalid");
        assert_eq!(
            error.values.get("reason").map(String::as_str),
            Some("credentials")
        );
    }

    #[test]
    fn manual_client_builds_for_every_supported_scheme() {
        for scheme in ["http", "https", "socks5", "socks5h"] {
            let candidate = settings(ProxyMode::Manual, &format!("{scheme}://127.0.0.1:1"), "");
            blocking_client("dsh-test", &candidate)
                .unwrap_or_else(|error| panic!("{scheme}: {error}"));
        }
        let with_bypass = settings(
            ProxyMode::Manual,
            "socks5://127.0.0.1:1080",
            "localhost; 127.0.0.1, internal.example",
        );
        blocking_client("dsh-test", &with_bypass).unwrap();
    }

    #[test]
    fn direct_and_system_clients_build() {
        blocking_client("dsh-test", &settings(ProxyMode::Direct, "", "")).unwrap();
        blocking_client("dsh-test", &ProxySettings::default()).unwrap();
    }

    // -----------------------------------------------------------------------
    // Sanitization and classification
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_redacts_userinfo_and_bounds_length() {
        assert_eq!(
            sanitize_detail("connect via http://user:secret@proxy.local:8080 failed"),
            "connect via http://***@proxy.local:8080 failed"
        );
        assert_eq!(
            sanitize_detail("error sending request for url (http://127.0.0.1:9/x)"),
            "error sending request"
        );
        assert_eq!(sanitize_detail("plain error"), "plain error");
        let long = "x".repeat(1000);
        let sanitized = sanitize_detail(&long);
        assert!(sanitized.chars().count() <= MAX_DETAIL_LEN + 1);
        assert!(sanitized.ends_with('…'));
    }

    #[test]
    fn classification_redacts_target_url_credentials() {
        let direct = settings(ProxyMode::Direct, "", "");
        let client = blocking_client("dsh-test", &direct).unwrap();
        // The target URL itself carries credentials here (never allowed for
        // real targets, but defensive): they must not survive classification.
        let error = client
            .get("http://user:topsecret@127.0.0.1:9/")
            .send()
            .unwrap_err();
        let classified = classify_reqwest(&error);
        assert!(
            !classified.detail.contains("topsecret"),
            "{}",
            classified.detail
        );
        assert!(
            !classified.detail.contains("user@"),
            "{}",
            classified.detail
        );
    }

    #[test]
    fn classification_distinguishes_connect_timeout_and_status() {
        let direct = settings(ProxyMode::Direct, "", "");
        let client = blocking_client("dsh-test", &direct).unwrap();

        let error = client.get(closed_loopback_url("/")).send().unwrap_err();
        assert_eq!(classify_reqwest(&error).kind, NetworkErrorKind::Connect);

        let mut sleeper = TestServer::spawn(|mut stream, stop| {
            let _ = read_request_head(&mut stream);
            // Answer eventually, but far past the client's 200ms timeout, and
            // stay interruptible so the server stops immediately on drop.
            for _ in 0..100 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let error = client
            .get(format!("{}/slow", sleeper.url()))
            .timeout(Duration::from_millis(200))
            .send()
            .unwrap_err();
        assert_eq!(classify_reqwest(&error).kind, NetworkErrorKind::Timeout);
        sleeper.stop();

        for (status, kind) in [
            (500_u16, NetworkErrorKind::HttpStatus),
            (407, NetworkErrorKind::ProxyAuth),
        ] {
            let phrase = if status == 407 {
                "407 Proxy Authentication Required"
            } else {
                "500 Internal Server Error"
            };
            let mut server = TestServer::spawn(move |mut stream, _stop| {
                let _ = read_request_head(&mut stream);
                let _ = stream.write_all(
                    format!("HTTP/1.1 {phrase}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                );
            });
            let error = client
                .get(format!("{}/status", server.url()))
                .send()
                .and_then(|response| response.error_for_status())
                .unwrap_err();
            assert_eq!(classify_reqwest(&error).kind, kind, "status {status}");
            server.stop();
        }
    }

    #[test]
    fn connect_tunnel_407_classifies_as_proxy_auth_without_a_status() {
        // A real local CONNECT proxy that immediately answers 407. The HTTPS
        // target is never resolved or contacted; reqwest reports the failure
        // as a request (tunnel) error whose source chain carries the explicit
        // proxy-auth message, with no status attached.
        let mut proxy = TestServer::spawn(|mut stream, _stop| {
            let head = read_request_head(&mut stream);
            assert!(
                String::from_utf8_lossy(&head).starts_with("CONNECT "),
                "expected a CONNECT request"
            );
            let _ = stream.write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
            );
        });
        let manual = settings(ProxyMode::Manual, &proxy.url(), "");
        let client = blocking_client("dsh-test", &manual).unwrap();
        let error = client
            .get("https://harness.invalid/packument")
            .send()
            .unwrap_err();
        let classified = classify_reqwest(&error);
        assert_eq!(error.status(), None);
        assert_eq!(
            classified.kind,
            NetworkErrorKind::ProxyAuth,
            "detail: {}",
            classified.detail
        );
        proxy.stop();
    }

    // -----------------------------------------------------------------------
    // Bypass normalization
    // -----------------------------------------------------------------------

    #[test]
    fn bypass_normalization_matches_reqwest_domain_grammar() {
        assert_eq!(normalized_bypass(""), None);
        assert_eq!(normalized_bypass(" , ; "), None);
        assert_eq!(
            normalized_bypass("localhost; 127.0.0.1 , internal.example"),
            Some("localhost,127.0.0.1,internal.example".to_owned())
        );
        // IE-style wildcards become equivalent leading-dot domain rules.
        assert_eq!(
            normalized_bypass("*.internal"),
            Some(".internal".to_owned())
        );
        assert_eq!(normalized_bypass("*"), Some("*".to_owned()));
        // Entries that are not hosts/domains are dropped, never echoed.
        assert_eq!(
            normalized_bypass("http://x; user@host; a b; *.corp"),
            Some(".corp".to_owned())
        );
    }

    // -----------------------------------------------------------------------
    // System plan resolution (per protocol, ALL fallback, WinInet fill)
    // -----------------------------------------------------------------------

    fn wininet(server: &str, override_list: &str) -> FakeSource {
        FakeSource(Some(WinInetProxy {
            enable: true,
            server: server.into(),
            override_list: override_list.into(),
        }))
    }

    #[test]
    fn system_plan_prefers_per_protocol_env_then_all_then_wininet() {
        // Only HTTPS_PROXY is set: Windows must fill http, not drop it.
        let source = wininet("http=win-http:8080;socks=win-socks:1080", "corp.local");
        let plan = system_proxy_plan(
            &lookup(&[("HTTPS_PROXY", "http://env-https:3128")]),
            Some(&source),
        );
        assert_eq!(plan.https.as_deref(), Some("http://env-https:3128"));
        assert_eq!(plan.http.as_deref(), Some("http://win-http:8080"));
        assert_eq!(plan.all.as_deref(), Some("socks5://win-socks:1080"));
        assert_eq!(plan.no_proxy.as_deref(), Some("corp.local"));
    }

    #[test]
    fn system_plan_all_proxy_fills_missing_protocols_before_wininet() {
        let source = wininet("http=win-http:8080", "");
        let plan = system_proxy_plan(
            &lookup(&[("ALL_PROXY", "socks5://env-all:1080")]),
            Some(&source),
        );
        assert_eq!(plan.http.as_deref(), Some("socks5://env-all:1080"));
        assert_eq!(plan.https.as_deref(), Some("socks5://env-all:1080"));
        assert_eq!(plan.all.as_deref(), Some("socks5://env-all:1080"));
    }

    #[test]
    fn system_plan_matches_both_env_casings_and_no_proxy_priority() {
        let source = wininet("127.0.0.1:7890", "win-bypass");
        let plan = system_proxy_plan(
            &lookup(&[("http_proxy", "http://lower:1"), ("no_proxy", "env-bypass")]),
            Some(&source),
        );
        assert_eq!(plan.http.as_deref(), Some("http://lower:1"));
        assert_eq!(plan.https.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(plan.no_proxy.as_deref(), Some("env-bypass"));

        let plan = system_proxy_plan(&lookup(&[]), Some(&source));
        assert_eq!(plan.no_proxy.as_deref(), Some("win-bypass"));
    }

    // -----------------------------------------------------------------------
    // Takeover decision: explicit per-protocol merging only on the Windows
    // path; elsewhere reqwest's default builder keeps env + OS proxy merging.
    // -----------------------------------------------------------------------

    #[test]
    fn takeover_only_happens_with_a_windows_source() {
        let env = lookup(&[("HTTPS_PROXY", "http://env-https:3128")]);
        // No platform source (macOS/Linux): default builder must be kept so
        // reqwest merges the environment with the OS system proxy itself.
        assert_eq!(takeover_system_plan(&env, None), None);
        // Windows source present: explicit takeover with the merged plan.
        let source = wininet("win-http:8080", "");
        let plan = takeover_system_plan(&env, Some(&source)).expect("takeover plan");
        assert_eq!(plan.https.as_deref(), Some("http://env-https:3128"));
        assert_eq!(plan.http.as_deref(), Some("http://win-http:8080"));
        // Windows source but nothing resolved anywhere: no takeover.
        let disabled = FakeSource(None);
        assert_eq!(takeover_system_plan(&lookup(&[]), Some(&disabled)), None);
    }

    // -----------------------------------------------------------------------
    // CGI safety: REQUEST_METHOD disables every proxy source.
    // -----------------------------------------------------------------------

    #[test]
    fn cgi_environment_resolves_no_proxies() {
        let env = lookup(&[
            ("REQUEST_METHOD", "GET"),
            ("HTTP_PROXY", "http://env-http:3128"),
            ("HTTPS_PROXY", "http://env-https:3128"),
            ("ALL_PROXY", "socks5://env-all:1080"),
        ]);
        let source = wininet("win-http:8080;socks=win-socks:1080", "corp.local");
        let plan = system_proxy_plan(&env, Some(&source));
        assert!(!plan.has_proxies(), "{plan:?}");
        assert_eq!(
            subprocess_env(&ProxySettings::default(), &env, Some(&source)),
            Vec::new()
        );
        assert_eq!(takeover_system_plan(&env, Some(&source)), None);
    }

    #[test]
    fn cgi_child_ignores_uppercase_http_proxy() {
        // Behavior-level proof: a CGI child with a recording proxy in
        // HTTP_PROXY must reach the target directly.
        let (mut proxy, requests) = fake_http_proxy();
        let mut target = serve_text("direct");
        let output = spawn_probe_child(
            &ProxySettings::default(),
            &target.url(),
            &[
                ("REQUEST_METHOD", "GET".to_owned()),
                ("HTTP_PROXY", proxy.url()),
                ("ALL_PROXY", proxy.url()),
            ],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=direct"), "{stdout}");
        target.stop();
        proxy.stop();
        assert!(requests.lock().expect("requests poisoned").is_empty());
    }

    // -----------------------------------------------------------------------
    // Environment proxy compatibility and casing fallback.
    // -----------------------------------------------------------------------

    #[test]
    fn schemeless_env_proxy_is_treated_as_http() {
        let env = lookup(&[("HTTP_PROXY", "proxy.corp:8080")]);
        let plan = system_proxy_plan(&env, None);
        assert_eq!(plan.http.as_deref(), Some("http://proxy.corp:8080"));
        let pairs = subprocess_env(&ProxySettings::default(), &env, None);
        assert!(pairs.contains(&(
            "HTTP_PROXY".to_owned(),
            OsString::from("http://proxy.corp:8080")
        )));
        // And env proxies are never overridden by WinInet fill-ins.
        let source = wininet("win-http:8080", "");
        let plan = system_proxy_plan(&env, Some(&source));
        assert_eq!(plan.http.as_deref(), Some("http://proxy.corp:8080"));
        assert_eq!(plan.https.as_deref(), Some("http://win-http:8080"));
    }

    #[test]
    fn invalid_or_empty_uppercase_falls_back_to_lowercase() {
        for (bad, good_key, good_value, expected) in [
            ("", "http_proxy", "http://lower:1", "http"),
            ("not a url", "https_proxy", "http://lower:2", "https"),
            ("::::", "all_proxy", "socks5://lower:3", "all"),
        ] {
            let pairs: Vec<(&str, &str)> = vec![
                (
                    match expected {
                        "http" => "HTTP_PROXY",
                        "https" => "HTTPS_PROXY",
                        _ => "ALL_PROXY",
                    },
                    bad,
                ),
                (good_key, good_value),
            ];
            let plan = system_proxy_plan(&lookup(&pairs), None);
            let resolved = match expected {
                "http" => &plan.http,
                "https" => &plan.https,
                _ => &plan.all,
            };
            assert_eq!(resolved.as_deref(), Some(good_value), "{bad}");
        }
        // NO_PROXY: empty/whitespace uppercase falls back to lowercase.
        let plan = system_proxy_plan(
            &lookup(&[("NO_PROXY", " , ; "), ("no_proxy", "corp.local")]),
            None,
        );
        assert_eq!(plan.no_proxy.as_deref(), Some("corp.local"));
    }

    #[test]
    fn bypass_normalization_keeps_cidr_and_rejects_the_rest() {
        assert_eq!(
            normalized_bypass("10.0.0.0/8"),
            Some("10.0.0.0/8".to_owned())
        );
        assert_eq!(
            normalized_bypass("2001:db8::/32"),
            Some("2001:db8::/32".to_owned())
        );
        // reqwest itself accepts these forms.
        assert!(reqwest::NoProxy::from_string("10.0.0.0/8").is_some());
        assert!(reqwest::NoProxy::from_string("2001:db8::/32").is_some());
        // Invalid prefixes, schemes, userinfo, paths, wildcards: dropped.
        assert_eq!(normalized_bypass("10.0.0.0/33"), None);
        assert_eq!(normalized_bypass("2001:db8::/129"), None);
        assert_eq!(normalized_bypass("10.0.0.0/x"), None);
        assert_eq!(normalized_bypass("foo/bar"), None);
        assert_eq!(normalized_bypass("a*"), None);
        assert_eq!(normalized_bypass("*x"), None);
        assert_eq!(normalized_bypass("*."), None);
    }

    #[test]
    fn system_plan_without_sources_stays_empty() {
        assert_eq!(
            system_proxy_plan(&lookup(&[]), None),
            SystemProxyPlan::default()
        );
        let disabled = FakeSource(Some(WinInetProxy {
            enable: false,
            server: "127.0.0.1:7890".into(),
            override_list: String::new(),
        }));
        assert_eq!(
            system_proxy_plan(&lookup(&[]), Some(&disabled)),
            SystemProxyPlan::default()
        );
        // Invalid env values are skipped, not forwarded.
        let plan = system_proxy_plan(&lookup(&[("HTTP_PROXY", "not a url")]), None);
        assert!(!plan.has_proxies());
    }

    // -----------------------------------------------------------------------
    // WinInet parsing (pure; never touches a real registry)
    // -----------------------------------------------------------------------

    #[test]
    fn wininet_single_address_applies_to_http_and_https() {
        let plan = system_proxy_plan(&lookup(&[]), Some(&wininet("proxy.corp:8080", "")));
        assert_eq!(plan.http.as_deref(), Some("http://proxy.corp:8080"));
        assert_eq!(plan.https.as_deref(), Some("http://proxy.corp:8080"));
        assert_eq!(plan.all, None);
    }

    #[test]
    fn wininet_per_protocol_addresses_map_to_their_variables() {
        let plan = system_proxy_plan(
            &lookup(&[]),
            Some(&wininet(
                "http=proxy.corp:8080;https=secure.corp:8443;socks=127.0.0.1:1080",
                "",
            )),
        );
        assert_eq!(plan.http.as_deref(), Some("http://proxy.corp:8080"));
        assert_eq!(plan.https.as_deref(), Some("http://secure.corp:8443"));
        assert_eq!(plan.all.as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(plan.no_proxy, None);
    }

    #[test]
    fn wininet_keeps_explicit_schemes_and_rejects_garbage() {
        let plan = system_proxy_plan(
            &lookup(&[]),
            Some(&wininet("http=http://proxy.corp:8080/;https=not a url", "")),
        );
        assert_eq!(plan.http.as_deref(), Some("http://proxy.corp:8080"));
        assert_eq!(plan.https, None);
    }

    #[test]
    fn wininet_disabled_or_empty_servers_produce_nothing() {
        let empty = wininet("  ", "<local>");
        assert_eq!(
            system_proxy_plan(&lookup(&[]), Some(&empty)),
            SystemProxyPlan {
                http: None,
                https: None,
                all: None,
                no_proxy: Some("localhost,127.0.0.1,::1".to_owned()),
            }
        );
    }

    #[test]
    fn wininet_override_expands_local_token_safely() {
        assert_eq!(
            wininet_bypass_list("<local>"),
            vec!["localhost", "127.0.0.1", "::1"]
        );
        assert_eq!(
            wininet_bypass_list("*.corp.local;<LOCAL>;proxy.corp"),
            vec![".corp.local", "localhost", "127.0.0.1", "::1", "proxy.corp"]
        );
        assert_eq!(wininet_bypass_list(""), Vec::<String>::new());
    }

    // -----------------------------------------------------------------------
    // Subprocess environment
    // -----------------------------------------------------------------------

    #[test]
    fn direct_mode_clears_every_proxy_variable() {
        let inherited = [
            ("HTTP_PROXY", "http://a"),
            ("http_proxy", "http://a"),
            ("HTTPS_PROXY", "http://a"),
            ("ALL_PROXY", "socks5://a"),
            ("all_proxy", "socks5://a"),
            ("NO_PROXY", "localhost"),
            ("no_proxy", "localhost"),
        ];
        let pairs = subprocess_env(
            &settings(ProxyMode::Direct, "", ""),
            &lookup(&inherited),
            None,
        );
        assert!(pairs.is_empty());
    }

    #[test]
    fn manual_mode_injects_every_casing_and_the_bypass_list() {
        let candidate = settings(
            ProxyMode::Manual,
            "http://127.0.0.1:7890",
            "localhost; 127.0.0.1",
        );
        let pairs = subprocess_env(&candidate, &lookup(&[]), None);
        let url = "http://127.0.0.1:7890/";
        let expected: Vec<(String, OsString)> = [
            ("HTTP_PROXY", url),
            ("http_proxy", url),
            ("HTTPS_PROXY", url),
            ("https_proxy", url),
            ("ALL_PROXY", url),
            ("all_proxy", url),
            ("NO_PROXY", "localhost,127.0.0.1"),
            ("no_proxy", "localhost,127.0.0.1"),
        ]
        .iter()
        .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
        .collect();
        assert_eq!(pairs, expected);
    }

    #[test]
    fn system_subprocess_env_merges_per_protocol_and_casings() {
        let source = wininet("http=win-http:8080;socks=win-socks:1080", "<local>");
        let pairs = subprocess_env(
            &ProxySettings::default(),
            &lookup(&[
                ("HTTPS_PROXY", "http://env-https:3128"),
                ("no_proxy", "env-only"),
            ]),
            Some(&source),
        );
        let find = |key: &str| {
            pairs.iter().find_map(|(name, value)| {
                (name == key).then(|| value.to_string_lossy().into_owned())
            })
        };
        // Per-protocol env wins for https; WinInet fills only http and all.
        assert_eq!(
            find("HTTPS_PROXY").as_deref(),
            Some("http://env-https:3128")
        );
        assert_eq!(
            find("https_proxy").as_deref(),
            Some("http://env-https:3128")
        );
        assert_eq!(find("HTTP_PROXY").as_deref(), Some("http://win-http:8080"));
        assert_eq!(find("http_proxy").as_deref(), Some("http://win-http:8080"));
        assert_eq!(
            find("ALL_PROXY").as_deref(),
            Some("socks5://win-socks:1080")
        );
        assert_eq!(
            find("all_proxy").as_deref(),
            Some("socks5://win-socks:1080")
        );
        // Env NO_PROXY wins over ProxyOverride.
        assert_eq!(find("NO_PROXY").as_deref(), Some("env-only"));
        assert_eq!(find("no_proxy").as_deref(), Some("env-only"));
    }

    #[test]
    fn system_subprocess_env_matches_client_plan() {
        let inherited = [("ALL_PROXY", "socks5://env-all:1080")];
        let source = wininet("http=win-http:8080", "corp.local");
        let plan = system_proxy_plan(&lookup(&inherited), Some(&source));
        let pairs = subprocess_env(
            &ProxySettings::default(),
            &lookup(&inherited),
            Some(&source),
        );
        let find = |key: &str| {
            pairs.iter().find_map(|(name, value)| {
                (name == key).then(|| value.to_string_lossy().into_owned())
            })
        };
        assert_eq!(find("HTTP_PROXY").as_deref(), plan.http.as_deref());
        assert_eq!(find("HTTPS_PROXY").as_deref(), plan.https.as_deref());
        assert_eq!(find("ALL_PROXY").as_deref(), plan.all.as_deref());
        assert_eq!(find("NO_PROXY").as_deref(), plan.no_proxy.as_deref());
    }

    #[test]
    fn proxy_env_key_matching_is_case_insensitive() {
        assert!(is_proxy_env_key("HTTP_PROXY"));
        assert!(is_proxy_env_key("Http_Proxy"));
        assert!(is_proxy_env_key("all_proxy"));
        assert!(!is_proxy_env_key("PATH"));
        assert!(!is_proxy_env_key("DSH_HOME"));
    }

    struct PanicSource;

    impl WindowsProxySource for PanicSource {
        fn read(&self) -> Option<WinInetProxy> {
            panic!("a unit test attempted to read the real platform proxy source")
        }
    }

    #[test]
    fn system_command_sources_are_disabled_without_isolated_opt_in() {
        let source = PanicSource;
        let pairs = command_subprocess_env(
            &ProxySettings::default(),
            &|_| panic!("a unit test attempted to read the host proxy environment"),
            Some(&source),
            false,
        );
        assert!(pairs.is_empty());
    }

    #[test]
    fn windows_test_build_never_exposes_the_real_registry_proxy_source() {
        assert!(platform_proxy_source().is_none());
    }

    // -----------------------------------------------------------------------
    // Behavioral tests. reqwest reads proxy environment variables at client
    // build time, so environment-dependent scenarios run in a child copy of
    // this test binary with a controlled, cleared environment. This keeps the
    // global process environment untouched and the tests parallel-safe.
    // -----------------------------------------------------------------------

    fn spawn_probe_child(
        settings: &ProxySettings,
        target: &str,
        extra: &[(&str, String)],
    ) -> std::process::Output {
        let executable = std::env::current_exe().unwrap();
        let mut command = Command::new(executable);
        command
            .args([
                "--ignored",
                "--exact",
                "network::tests::proxy_probe_child",
                "--nocapture",
            ])
            .env_clear()
            .env(
                "DSH_PROXY_TEST_SETTINGS",
                serde_json::to_string(settings).unwrap(),
            )
            .env("DSH_PROXY_TEST_TARGET", target)
            // Explicit, isolated opt-in: the test-only client path never
            // discovers the host OS proxy and reads only variables in this
            // env-cleared child.
            .env(TEST_SYSTEM_PROXY_ENV, "1")
            .envs(extra.iter().map(|(key, value)| (*key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().unwrap()
    }

    #[test]
    #[ignore]
    fn proxy_probe_child() {
        let settings: ProxySettings =
            serde_json::from_str(&std::env::var("DSH_PROXY_TEST_SETTINGS").unwrap()).unwrap();
        let target = std::env::var("DSH_PROXY_TEST_TARGET").unwrap();
        let client = blocking_client("dsh-proxy-test", &settings).unwrap();
        match client
            .get(&target)
            .timeout(Duration::from_secs(10))
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.text())
        {
            Ok(body) => println!("PROBE_OK={body}"),
            Err(error) => {
                let classified = classify_reqwest(&error);
                println!("PROBE_ERR_KIND={}", classified.kind.as_str());
                println!("PROBE_ERR={}", classified.detail);
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn manual_mode_routes_requests_through_the_proxy() {
        let (mut proxy, requests) = fake_http_proxy();
        let target = closed_loopback_url("/packument");
        let output =
            spawn_probe_child(&settings(ProxyMode::Manual, &proxy.url(), ""), &target, &[]);
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=proxied"), "{stdout}");
        proxy.stop();
        let requests = requests.lock().expect("requests poisoned");
        assert_eq!(requests.len(), 1);
        // Proxied plain-HTTP requests carry the absolute target URI.
        assert!(
            requests[0].starts_with(&format!("GET {target} ")),
            "request line: {}",
            requests[0]
        );
    }

    #[test]
    fn manual_mode_bypass_list_skips_the_proxy() {
        let (mut proxy, requests) = fake_http_proxy();
        let mut target = serve_text("direct");
        let output = spawn_probe_child(
            &settings(ProxyMode::Manual, &proxy.url(), "127.0.0.1"),
            &target.url(),
            &[],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=direct"), "{stdout}");
        target.stop();
        proxy.stop();
        assert!(requests.lock().expect("requests poisoned").is_empty());
    }

    #[test]
    fn manual_mode_wildcard_bypass_matches_domain_and_subdomains() {
        let (mut proxy, requests) = fake_http_proxy();
        // `localhost` resolves to loopback, so this exercises reqwest's real
        // NoProxy domain matching through the public client behavior.
        let mut target = serve_text("direct");
        let target_url = target.url().replace("127.0.0.1", "localhost");
        let output = spawn_probe_child(
            &settings(ProxyMode::Manual, &proxy.url(), "*.localhost"),
            &target_url,
            &[],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=direct"), "{stdout}");
        target.stop();
        proxy.stop();
        assert!(requests.lock().expect("requests poisoned").is_empty());
    }

    #[test]
    fn manual_mode_wildcard_bypass_does_not_match_other_domains() {
        let (mut proxy, requests) = fake_http_proxy();
        let target = closed_loopback_url("/not-bypassed");
        let output = spawn_probe_child(
            &settings(ProxyMode::Manual, &proxy.url(), "*.internal.example"),
            &target,
            &[],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        proxy.stop();
        // 127.0.0.1 is not under .internal.example: the request went through.
        assert_eq!(requests.lock().expect("requests poisoned").len(), 1);
    }

    #[test]
    fn direct_mode_ignores_proxy_environment_variables() {
        let mut target = serve_text("direct");
        let dead_proxy = closed_loopback_url("");
        let output = spawn_probe_child(
            &settings(ProxyMode::Direct, "", ""),
            &target.url(),
            &[
                ("HTTP_PROXY", dead_proxy.clone()),
                ("http_proxy", dead_proxy.clone()),
                ("HTTPS_PROXY", dead_proxy.clone()),
                ("https_proxy", dead_proxy.clone()),
                ("ALL_PROXY", dead_proxy.clone()),
                ("all_proxy", dead_proxy),
            ],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=direct"), "{stdout}");
        target.stop();
    }

    #[test]
    fn system_mode_respects_proxy_environment_variables() {
        let (mut proxy, requests) = fake_http_proxy();
        let target = closed_loopback_url("/via-env");
        let output = spawn_probe_child(
            &ProxySettings::default(),
            &target,
            &[("HTTP_PROXY", proxy.url())],
        );
        assert!(
            output.status.success(),
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PROBE_OK=proxied"), "{stdout}");
        proxy.stop();
        assert_eq!(requests.lock().expect("requests poisoned").len(), 1);
    }
}
