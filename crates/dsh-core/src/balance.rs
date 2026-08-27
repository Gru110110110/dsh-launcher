//! Official DeepSeek balance support using a minimal Harness bridge.
//!
//! The launcher never reads API keys. It stages a self-contained Cordis
//! module that resolves the effective credential inside Harness, calls the
//! fixed official balance endpoint, and exposes only a sanitized result over
//! a token-guarded loopback socket.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;
use uuid::Uuid;

use crate::{AppError, AppResult, ApplicationPaths, paths::atomic_write};

const BRIDGE_SOURCE: &str = include_str!("../bridge/balance-bridge.mjs");

pub const BALANCE_LISTEN_ENV: &str = "DSH_DESKTOP_BALANCE_LISTEN";
pub const BALANCE_TOKEN_ENV: &str = "DSH_DESKTOP_BALANCE_TOKEN";
pub const BALANCE_OVERLAY_ENV: &str = "DSH_DESKTOP_BALANCE_OVERLAY";

const SYNTAX_CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);
// The bridge allows the official upstream request up to 10 seconds. Keep the
// desktop timeout above that boundary so a successful bridge response is not
// reported as a launcher-side failure.
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(12);
const BRIDGE_MAX_BYTES: u64 = 64 * 1024;
const MAX_DETAIL_LEN: usize = 200;
const MAX_BALANCE_LEN: usize = 32;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum BalanceStatus {
    Ok,
    Stale,
    #[default]
    Unavailable,
}

/// A stale response carries the last successful amount so the UI can keep it
/// visible while reporting the failed query only through a toast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BalanceSnapshot {
    pub status: BalanceStatus,
    pub detail: Option<String>,
    pub is_available: Option<bool>,
    pub currency: Option<String>,
    pub total_balance: Option<String>,
    #[ts(type = "number | null")]
    pub fetched_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceBridgeEndpoint {
    port: u16,
    token: String,
}

impl BalanceBridgeEndpoint {
    fn new(port: u16, token: String) -> Self {
        Self { port, token }
    }

    pub fn listen_addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

fn allocate_loopback_port() -> AppResult<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| AppError::io("balanceBridgePortFailed", &error))?;
    Ok(listener.local_addr()?.port())
}

fn generate_bridge_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[derive(Debug, Clone, Default)]
pub struct BalanceLaunchPlan {
    pub overlay: Option<std::path::PathBuf>,
    pub endpoint: Option<BalanceBridgeEndpoint>,
    pub env: Vec<(String, OsString)>,
    pub unavailable_reason: Option<&'static str>,
}

impl BalanceLaunchPlan {
    pub fn disabled(reason: &'static str) -> Self {
        Self {
            unavailable_reason: Some(reason),
            ..Self::default()
        }
    }

    pub fn overlay_active(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn without_overlay(&self) -> Self {
        Self::disabled("balanceBridgeStartFailed")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreflightCache {
    harness_version: String,
    module_sha256: String,
    overlay_sha256: String,
}

/// Prepare the optional bridge. Failure disables balance without blocking the
/// Harness service itself.
pub fn prepare_balance_launch(paths: &ApplicationPaths) -> BalanceLaunchPlan {
    match prepare_balance_launch_inner(paths) {
        Ok(plan) => plan,
        Err(error) => {
            log::warn!("balance bridge staging failed; starting without it: {error}");
            BalanceLaunchPlan::disabled("balanceBridgeStagingFailed")
        }
    }
}

fn prepare_balance_launch_inner(paths: &ApplicationPaths) -> AppResult<BalanceLaunchPlan> {
    stage_bridge_module(paths)?;
    syntax_check_module(paths)?;
    let overlay = stage_overlay(paths)?;
    if !overlay_preflight(paths, &overlay)? {
        return Ok(BalanceLaunchPlan::disabled(
            "balanceBridgeOverlayUnsupported",
        ));
    }
    let endpoint = BalanceBridgeEndpoint::new(allocate_loopback_port()?, generate_bridge_token());
    let env = vec![
        (BALANCE_LISTEN_ENV.to_owned(), endpoint.listen_addr().into()),
        (BALANCE_TOKEN_ENV.to_owned(), endpoint.token.clone().into()),
        (
            BALANCE_OVERLAY_ENV.to_owned(),
            overlay.clone().into_os_string(),
        ),
    ];
    Ok(BalanceLaunchPlan {
        overlay: Some(overlay),
        endpoint: Some(endpoint),
        env,
        unavailable_reason: None,
    })
}

fn stage_bridge_module(paths: &ApplicationPaths) -> AppResult<()> {
    let current = fs::read(&paths.balance_bridge_module).unwrap_or_default();
    if current != BRIDGE_SOURCE.as_bytes() {
        atomic_write(&paths.balance_bridge_module, BRIDGE_SOURCE.as_bytes())?;
    }
    Ok(())
}

fn syntax_check_module(paths: &ApplicationPaths) -> AppResult<()> {
    let mut command = Command::new(&paths.node_bin);
    command.arg("--check").arg(&paths.balance_bridge_module);
    run_quiet(&mut command, SYNTAX_CHECK_TIMEOUT)
        .map_err(|error| AppError::new("balanceBridgeSyntaxInvalid").detail(error.to_string()))
}

fn stage_overlay(paths: &ApplicationPaths) -> AppResult<std::path::PathBuf> {
    let module_url = url::Url::from_file_path(&paths.balance_bridge_module)
        .map_err(|_| AppError::new("invalidPath"))?;
    let overlay = format!(
        "# DSH Launcher balance bridge overlay. Generated on service start; do not edit.\n\
         # Injected through `dsh web --patch`; it never changes the user's Harness profile.\n\
         - insert:\n\
         \x20   - id: dsh-desktop-balance-bridge\n\
         \x20     name: \"{module_url}\"\n"
    );
    let current = fs::read(&paths.balance_bridge_overlay).unwrap_or_default();
    if current != overlay.as_bytes() {
        atomic_write(&paths.balance_bridge_overlay, overlay.as_bytes())?;
    }
    Ok(paths.balance_bridge_overlay.clone())
}

fn overlay_preflight(paths: &ApplicationPaths, overlay: &Path) -> AppResult<bool> {
    let cache_key = PreflightCache {
        harness_version: crate::runtime::installed_version(paths).unwrap_or_default(),
        module_sha256: hex::encode(Sha256::digest(BRIDGE_SOURCE.as_bytes())),
        overlay_sha256: hex::encode(Sha256::digest(fs::read(overlay)?)),
    };
    if let Ok(bytes) = fs::read(&paths.balance_bridge_preflight)
        && serde_json::from_slice::<PreflightCache>(&bytes).is_ok_and(|cached| {
            cached.harness_version == cache_key.harness_version
                && cached.module_sha256 == cache_key.module_sha256
                && cached.overlay_sha256 == cache_key.overlay_sha256
        })
    {
        return Ok(true);
    }
    let mut command = Command::new(&paths.node_bin);
    command
        .arg(&paths.dsh_bin)
        .arg("web")
        .arg("--patch")
        .arg(overlay)
        .arg("--dump-config")
        .env("DSH_HOME", &paths.dsh_home);
    if run_quiet(&mut command, PREFLIGHT_TIMEOUT).is_err() {
        return Ok(false);
    }
    atomic_write(
        &paths.balance_bridge_preflight,
        serde_json::to_vec(&cache_key)?.as_slice(),
    )?;
    Ok(true)
}

fn run_quiet(command: &mut Command, timeout: Duration) -> AppResult<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| AppError::io("balanceBridgePreflightFailed", &error))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(AppError::new("balanceBridgePreflightFailed")
                    .value("status", status.to_string()));
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::new("balanceBridgePreflightTimedOut"));
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBalance {
    version: u32,
    status: String,
    detail: Option<String>,
    is_available: Option<bool>,
    currency: Option<String>,
    total_balance: Option<String>,
    fetched_at_ms: Option<u64>,
}

const BALANCE_DETAIL_CODES: &[&str] = &[
    "balanceNoCredential",
    "balanceNonOfficialEndpoint",
    "balanceHttpError",
    "balanceTimeout",
    "balanceInvalidResponse",
    "balanceUnavailable",
];

fn valid_balance(value: &str) -> bool {
    let mut parts = value.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() || integer.is_empty() {
        return false;
    }
    let digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    let signed = integer.strip_prefix('-').unwrap_or(integer);
    value.len() <= MAX_BALANCE_LEN
        && digits(signed)
        && fraction.is_none_or(|part| part.len() <= 8 && digits(part))
}

fn sanitize_detail(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DETAIL_LEN)
        .collect()
}

fn sanitize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| sanitize_detail(&value))
        .filter(|value| !value.is_empty())
}

fn parse_wire_balance(bytes: &[u8]) -> AppResult<WireBalance> {
    let wire: WireBalance = serde_json::from_slice(bytes)?;
    let status_ok = matches!(wire.status.as_str(), "ok" | "stale" | "unavailable");
    let detail_ok = wire
        .detail
        .as_deref()
        .is_none_or(|code| BALANCE_DETAIL_CODES.contains(&code));
    let currency_ok = wire.currency.as_deref().is_none_or(|currency| {
        !currency.is_empty()
            && currency.len() <= 8
            && currency.chars().all(|c| c.is_ascii_alphabetic())
    });
    let balance_ok = wire.total_balance.as_deref().is_none_or(valid_balance);
    if wire.version != 1 || !status_ok || !detail_ok || !currency_ok || !balance_ok {
        return Err(AppError::new("balanceBridgeResponseInvalid"));
    }
    Ok(wire)
}

fn fetch_bridge(endpoint: &BalanceBridgeEndpoint, path: &str) -> AppResult<Vec<u8>> {
    let url = endpoint.url(path);
    let parsed = url::Url::parse(&url).map_err(|_| AppError::new("balanceBridgeAddressInvalid"))?;
    let loopback = matches!(parsed.host(), Some(url::Host::Ipv4(ip)) if ip == Ipv4Addr::LOCALHOST);
    if !loopback {
        return Err(AppError::new("balanceBridgeAddressInvalid"));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(BRIDGE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| AppError::new("balanceBridgeClientFailed"))?;
    let response = client
        .get(&url)
        .header("x-dsh-balance-token", &endpoint.token)
        .send()
        .map_err(|_| AppError::new("balanceFetchFailed"))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(AppError::new("balanceFetchFailed"));
    }
    let mut body = Vec::new();
    response
        .take(BRIDGE_MAX_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| AppError::new("balanceFetchFailed"))?;
    if body.len() as u64 > BRIDGE_MAX_BYTES {
        return Err(AppError::new("balanceBridgeResponseTooLarge"));
    }
    Ok(body)
}

pub struct BalanceService {
    last_good: Mutex<Option<BalanceSnapshot>>,
}

impl Default for BalanceService {
    fn default() -> Self {
        Self::new()
    }
}

impl BalanceService {
    pub fn new() -> Self {
        Self {
            last_good: Mutex::new(None),
        }
    }

    pub fn snapshot(
        &self,
        endpoint: Option<&BalanceBridgeEndpoint>,
        force_refresh: bool,
    ) -> BalanceSnapshot {
        let Some(endpoint) = endpoint else {
            return self.stale_or_unavailable("balanceBridgeUnavailable");
        };
        let path = if force_refresh {
            "/balance?refresh=1"
        } else {
            "/balance"
        };
        let wire = fetch_bridge(endpoint, path).and_then(|bytes| parse_wire_balance(&bytes));
        let Ok(wire) = wire else {
            return self.stale_or_unavailable("balanceFetchFailed");
        };
        let mapped = BalanceSnapshot {
            status: match wire.status.as_str() {
                "ok" => BalanceStatus::Ok,
                "stale" => BalanceStatus::Stale,
                _ => BalanceStatus::Unavailable,
            },
            detail: sanitize_optional(wire.detail),
            is_available: wire.is_available,
            currency: wire.currency,
            total_balance: wire.total_balance,
            fetched_at_ms: wire.fetched_at_ms,
        };
        if mapped.status == BalanceStatus::Ok {
            *self.last_good.lock().expect("balance cache poisoned") = Some(mapped.clone());
            return mapped;
        }
        if mapped.total_balance.is_some() {
            return mapped;
        }
        self.stale_or_unavailable(mapped.detail.as_deref().unwrap_or("balanceUnavailable"))
    }

    fn stale_or_unavailable(&self, detail: &str) -> BalanceSnapshot {
        let cached = self
            .last_good
            .lock()
            .expect("balance cache poisoned")
            .clone();
        match cached {
            Some(mut balance) => {
                balance.status = BalanceStatus::Stale;
                balance.detail = Some(detail.into());
                balance
            }
            None => BalanceSnapshot {
                status: BalanceStatus::Unavailable,
                detail: Some(detail.into()),
                is_available: None,
                currency: None,
                total_balance: None,
                fetched_at_ms: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpStream,
        sync::{Arc, Mutex},
    };

    use super::*;

    fn serve_responses(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (
        BalanceBridgeEndpoint,
        Arc<Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake bridge");
        let port = listener.local_addr().expect("fake bridge address").port();
        let token = "test-balance-token".to_owned();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let expected_token = token.clone();
        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept fake request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone fake stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read request line");
                seen.lock()
                    .expect("request list poisoned")
                    .push(request_line.trim().to_owned());
                let mut token_seen = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read request header");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if lower.starts_with("x-dsh-balance-token:")
                        && line
                            .split_once(':')
                            .is_some_and(|(_, value)| value.trim() == expected_token)
                    {
                        token_seen = true;
                    }
                }
                assert!(
                    token_seen,
                    "desktop must authenticate to the loopback bridge"
                );
                write_response(&mut stream, status, body);
            }
        });
        (BalanceBridgeEndpoint::new(port, token), requests, handle)
    }

    fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        )
        .expect("write fake response");
        stream.flush().expect("flush fake response");
    }

    #[test]
    fn wire_contract_preserves_official_decimal_text_and_rejects_malformed_values() {
        let wire = parse_wire_balance(
            br#"{"version":1,"status":"ok","detail":null,"isAvailable":true,"currency":"CNY","totalBalance":"46.5700","fetchedAtMs":123}"#,
        )
        .expect("valid balance wire payload");
        assert_eq!(wire.total_balance.as_deref(), Some("46.5700"));

        for invalid in [
            br#"{"version":2,"status":"ok","totalBalance":"1.00"}"#.as_slice(),
            br#"{"version":1,"status":"mystery","totalBalance":"1.00"}"#.as_slice(),
            br#"{"version":1,"status":"ok","totalBalance":"0.000000000"}"#.as_slice(),
            br#"{"version":1,"status":"ok","currency":"CNY\n","totalBalance":"1.00"}"#.as_slice(),
        ] {
            assert!(parse_wire_balance(invalid).is_err());
        }
    }

    #[test]
    fn refresh_bypasses_cache_and_failure_keeps_last_success_without_zero() {
        let ok = r#"{"version":1,"status":"ok","detail":null,"isAvailable":true,"currency":"CNY","totalBalance":"46.5700","fetchedAtMs":123}"#;
        let (endpoint, requests, server) = serve_responses(vec![
            ("200 OK", ok),
            ("503 Service Unavailable", r#"{"error":"offline"}"#),
        ]);
        let service = BalanceService::new();

        let first = service.snapshot(Some(&endpoint), false);
        assert_eq!(first.status, BalanceStatus::Ok);
        assert_eq!(first.total_balance.as_deref(), Some("46.5700"));

        let stale = service.snapshot(Some(&endpoint), true);
        assert_eq!(stale.status, BalanceStatus::Stale);
        assert_eq!(stale.detail.as_deref(), Some("balanceFetchFailed"));
        assert_eq!(stale.total_balance.as_deref(), Some("46.5700"));

        server.join().expect("fake bridge thread");
        assert_eq!(
            *requests.lock().expect("request list poisoned"),
            ["GET /balance HTTP/1.1", "GET /balance?refresh=1 HTTP/1.1"]
        );
    }

    #[test]
    fn unavailable_bridge_never_synthesizes_a_zero_balance() {
        let snapshot = BalanceService::new().snapshot(None, false);
        assert_eq!(snapshot.status, BalanceStatus::Unavailable);
        assert_eq!(snapshot.detail.as_deref(), Some("balanceBridgeUnavailable"));
        assert!(snapshot.total_balance.is_none());
        assert!(snapshot.currency.is_none());
    }
}
