use std::{
    io::{BufRead, Read},
    net::{Ipv4Addr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::{AppError, AppResult};

pub const PET_LISTEN_ENV: &str = "DSH_DESKTOP_PET_LISTEN";
pub const PET_TOKEN_ENV: &str = "DSH_DESKTOP_PET_TOKEN";

const MAX_EVENT_LINE: usize = 64 * 1024;
const MAX_PHASE_LEN: usize = 80;
const MAX_ACTIVITY_LEN: usize = 40;
const MAX_TOOL_LEN: usize = 80;
const MAX_PROJECT_LEN: usize = 40;
const MAX_TASK_LEN: usize = 120;

fn default_selected_pet_id() -> String {
    "marmot".into()
}

fn default_pet_scale() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PetPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PetPreferences {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_selected_pet_id")]
    pub selected_pet_id: String,
    #[serde(default = "default_pet_scale")]
    pub scale: f64,
    #[serde(default = "default_true")]
    pub bubble_enabled: bool,
    #[serde(default)]
    pub click_through: bool,
    #[serde(default)]
    pub reduced_motion: bool,
    #[serde(default)]
    pub position: Option<PetPosition>,
}

impl Default for PetPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            selected_pet_id: default_selected_pet_id(),
            scale: default_pet_scale(),
            bubble_enabled: true,
            click_through: false,
            reduced_motion: false,
            position: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PetPreferencesPatch {
    pub enabled: Option<bool>,
    pub selected_pet_id: Option<String>,
    pub scale: Option<f64>,
    pub bubble_enabled: Option<bool>,
    pub click_through: Option<bool>,
    pub reduced_motion: Option<bool>,
    pub position: Option<PetPosition>,
}

impl PetPreferences {
    pub fn apply_patch(&mut self, patch: PetPreferencesPatch) -> AppResult<()> {
        if let Some(enabled) = patch.enabled {
            self.enabled = enabled;
        }
        if let Some(selected_pet_id) = patch.selected_pet_id {
            let valid = !selected_pet_id.is_empty()
                && selected_pet_id.len() <= 48
                && selected_pet_id.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'-'
                        || byte == b'_'
                });
            if !valid {
                return Err(AppError::new("petSelectionInvalid"));
            }
            self.selected_pet_id = selected_pet_id;
        }
        if let Some(scale) = patch.scale {
            if !scale.is_finite() || !(0.65..=1.4).contains(&scale) {
                return Err(AppError::new("petScaleInvalid"));
            }
            self.scale = scale;
        }
        if let Some(bubble_enabled) = patch.bubble_enabled {
            self.bubble_enabled = bubble_enabled;
        }
        if let Some(click_through) = patch.click_through {
            self.click_through = click_through;
        }
        if let Some(reduced_motion) = patch.reduced_motion {
            self.reduced_motion = reduced_motion;
        }
        if let Some(position) = patch.position {
            self.position = Some(position);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PetBridgeEndpoint {
    port: u16,
    token: String,
}

impl PetBridgeEndpoint {
    pub fn allocate() -> AppResult<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| AppError::io("petBridgePortFailed", &error))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            port,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        })
    }

    #[cfg(test)]
    fn new(port: u16, token: impl Into<String>) -> Self {
        Self {
            port,
            token: token.into(),
        }
    }

    pub fn listen_addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Query the host directly: a cached idle SSE event can predate a new turn.
    /// Unknown or failed probes never authorize an automatic restart.
    pub fn confirms_idle(&self) -> bool {
        let probe = || -> AppResult<bool> {
            let response = reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .map_err(|_| AppError::new("petBridgeUnavailable"))?
                .get(format!("http://127.0.0.1:{}/pet/state", self.port))
                .header("x-dsh-pet-token", &self.token)
                .send()
                .and_then(|response| response.error_for_status())
                .map_err(|_| AppError::new("petBridgeUnavailable"))?;
            let mut bytes = Vec::new();
            response
                .take(MAX_EVENT_LINE as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_EVENT_LINE {
                return Ok(false);
            }
            Ok(parse_wire_snapshot(&bytes)?.state == PetState::Idle)
        };
        probe().unwrap_or(false)
    }

    fn events_url(&self, since: u64) -> String {
        format!("http://127.0.0.1:{}/pet/events?since={since}", self.port)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum PetState {
    Waiting,
    Error,
    Working,
    Thinking,
    #[default]
    Idle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum PetBridgeStatus {
    Connected,
    Stale,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PetProgress {
    pub completed: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PetSnapshot {
    pub bridge_status: PetBridgeStatus,
    pub state: PetState,
    pub phase: String,
    pub activity: Option<String>,
    pub tool_name: Option<String>,
    pub project: Option<String>,
    pub task: Option<String>,
    pub progress: Option<PetProgress>,
    #[ts(type = "number")]
    pub sequence: u64,
    #[ts(type = "number | null")]
    pub updated_at_ms: Option<u64>,
}

impl Default for PetSnapshot {
    fn default() -> Self {
        Self {
            bridge_status: PetBridgeStatus::Unavailable,
            state: PetState::Idle,
            phase: "bridge-unavailable".into(),
            activity: None,
            tool_name: None,
            project: None,
            task: None,
            progress: None,
            sequence: 0,
            updated_at_ms: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireSnapshot {
    version: u8,
    sequence: u64,
    state: PetState,
    phase: String,
    activity: Option<String>,
    tool_name: Option<String>,
    project: Option<String>,
    task: Option<String>,
    progress: Option<PetProgress>,
    updated_at_ms: u64,
}

fn valid_text(value: &Option<String>, max: usize) -> bool {
    value.as_ref().is_none_or(|text| {
        !text.chars().any(char::is_control) && !text.is_empty() && text.chars().count() <= max
    })
}

fn parse_wire_snapshot(bytes: &[u8]) -> AppResult<PetSnapshot> {
    let wire: WireSnapshot =
        serde_json::from_slice(bytes).map_err(|_| AppError::new("petBridgePayloadInvalid"))?;
    if wire.version != 1
        || wire.phase.is_empty()
        || wire.phase.chars().any(char::is_control)
        || wire.phase.chars().count() > MAX_PHASE_LEN
        || !valid_text(&wire.activity, MAX_ACTIVITY_LEN)
        || !valid_text(&wire.tool_name, MAX_TOOL_LEN)
        || !valid_text(&wire.project, MAX_PROJECT_LEN)
        || !valid_text(&wire.task, MAX_TASK_LEN)
        || wire
            .progress
            .as_ref()
            .is_some_and(|progress| progress.total == 0 || progress.completed > progress.total)
    {
        return Err(AppError::new("petBridgePayloadInvalid"));
    }
    Ok(PetSnapshot {
        bridge_status: PetBridgeStatus::Connected,
        state: wire.state,
        phase: wire.phase,
        activity: wire.activity,
        tool_name: wire.tool_name,
        project: wire.project,
        task: wire.task,
        progress: wire.progress,
        sequence: wire.sequence,
        updated_at_ms: Some(wire.updated_at_ms),
    })
}

pub type PetListener = Arc<dyn Fn(PetSnapshot) + Send + Sync + 'static>;

pub struct PetService {
    snapshot: Arc<Mutex<PetSnapshot>>,
    listener: PetListener,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PetService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PetService")
            .field("snapshot", &self.snapshot())
            .field("running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for PetService {
    fn default() -> Self {
        Self::new(Arc::new(|_| {}))
    }
}

impl PetService {
    pub fn new(listener: PetListener) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(PetSnapshot::default())),
            listener,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    pub fn snapshot(&self) -> PetSnapshot {
        self.snapshot.lock().expect("pet snapshot poisoned").clone()
    }

    pub fn start(&mut self, endpoint: Option<PetBridgeEndpoint>) {
        self.stop_worker();
        let Some(endpoint) = endpoint else {
            self.publish(PetSnapshot::default());
            return;
        };
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&self.stop);
        let snapshot = Arc::clone(&self.snapshot);
        let listener = Arc::clone(&self.listener);
        self.worker = thread::Builder::new()
            .name("pet-bridge-events".into())
            .spawn(move || run_event_worker(endpoint, stop, snapshot, listener))
            .map_err(|error| log::warn!("pet bridge worker could not start: {error}"))
            .ok();
        if self.worker.is_none() {
            self.publish(PetSnapshot::default());
        }
    }

    pub fn stop(&mut self) {
        self.stop_worker();
        self.publish(PetSnapshot::default());
    }

    fn publish(&self, value: PetSnapshot) {
        *self.snapshot.lock().expect("pet snapshot poisoned") = value.clone();
        (self.listener)(value);
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(worker) = self.worker.take() else {
            return;
        };
        for _ in 0..40 {
            if worker.is_finished() {
                let _ = worker.join();
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        log::warn!("pet bridge event worker did not stop before its cleanup deadline");
    }
}

impl Drop for PetService {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

fn publish_if_changed(
    snapshot: &Arc<Mutex<PetSnapshot>>,
    listener: &PetListener,
    update: impl FnOnce(&mut PetSnapshot) -> bool,
) {
    let value = {
        let mut current = snapshot.lock().expect("pet snapshot poisoned");
        if !update(&mut current) {
            return;
        }
        current.clone()
    };
    listener(value);
}

fn run_event_worker(
    endpoint: PetBridgeEndpoint,
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<PetSnapshot>>,
    listener: PetListener,
) {
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            log::warn!("pet bridge client could not be built: {error}");
            return;
        }
    };
    let mut delay = Duration::from_millis(200);
    while !stop.load(Ordering::SeqCst) {
        let since = snapshot.lock().expect("pet snapshot poisoned").sequence;
        let response = client
            .get(endpoint.events_url(since))
            .header("x-dsh-pet-token", &endpoint.token)
            .send();
        let result = response
            .map_err(|_| AppError::new("petBridgeUnavailable"))
            .and_then(|response| {
                if !response.status().is_success() {
                    return Err(AppError::new("petBridgeUnavailable"));
                }
                read_sse(response, &stop, &snapshot, &listener)
            });
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if result.is_err() {
            publish_if_changed(&snapshot, &listener, |current| {
                let next = if current.updated_at_ms.is_some() {
                    PetBridgeStatus::Stale
                } else {
                    PetBridgeStatus::Unavailable
                };
                if current.bridge_status == next {
                    return false;
                }
                current.bridge_status = next;
                true
            });
        } else {
            delay = Duration::from_millis(200);
        }
        let mut waited = Duration::ZERO;
        while waited < delay && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
        delay = (delay * 2).min(Duration::from_secs(3));
    }
}

fn read_sse(
    response: reqwest::blocking::Response,
    stop: &AtomicBool,
    snapshot: &Arc<Mutex<PetSnapshot>>,
    listener: &PetListener,
) -> AppResult<()> {
    read_sse_stream(std::io::BufReader::new(response), stop, snapshot, listener)
}

fn read_sse_stream(
    mut reader: impl BufRead,
    stop: &AtomicBool,
    snapshot: &Arc<Mutex<PetSnapshot>>,
    listener: &PetListener,
) -> AppResult<()> {
    let mut line = String::new();
    let mut data = String::new();
    let mut connection_announced = false;
    while !stop.load(Ordering::SeqCst) {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| AppError::new("petBridgeUnavailable"))?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_EVENT_LINE {
            return Err(AppError::new("petBridgePayloadTooLarge"));
        }
        if !connection_announced {
            publish_if_changed(snapshot, listener, |current| {
                if current.bridge_status == PetBridgeStatus::Connected {
                    return false;
                }
                current.bridge_status = PetBridgeStatus::Connected;
                true
            });
            connection_announced = true;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(value) = trimmed.strip_prefix("data:") {
            data.push_str(value.trim_start());
            if data.len() > MAX_EVENT_LINE {
                return Err(AppError::new("petBridgePayloadTooLarge"));
            }
            continue;
        }
        if !trimmed.is_empty() || data.is_empty() {
            continue;
        }
        let value = parse_wire_snapshot(data.as_bytes())?;
        data.clear();
        publish_if_changed(snapshot, listener, |current| {
            if value.sequence <= current.sequence
                && current.bridge_status == PetBridgeStatus::Connected
            {
                return false;
            }
            *current = value;
            true
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_probe_requires_current_idle_state_and_fails_closed() {
        use std::io::Write;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = PetBridgeEndpoint::new(listener.local_addr().unwrap().port(), "test-only");
        let worker = thread::spawn(move || {
            for state in ["working", "thinking", "waiting", "error", "idle", "invalid"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                assert!(request.starts_with("GET /pet/state "));
                assert!(request.contains("x-dsh-pet-token: test-only"));
                let body = format!(
                    r#"{{"version":1,"sequence":1,"state":"{state}","phase":"test","updatedAtMs":1}}"#
                );
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        for expected in [false, false, false, false, true, false] {
            assert_eq!(endpoint.confirms_idle(), expected);
        }
        worker.join().unwrap();
        assert!(
            !endpoint.confirms_idle(),
            "unreachable bridge must not imply idle"
        );
    }

    #[test]
    fn parses_a_bounded_five_state_snapshot() {
        let value = parse_wire_snapshot(
            br#"{"version":1,"sequence":4,"state":"working","phase":"tool-call","activity":"testing","project":"demo","task":"run tests","progress":{"completed":1,"total":3},"updatedAtMs":42}"#,
        )
        .unwrap();
        assert_eq!(value.bridge_status, PetBridgeStatus::Connected);
        assert_eq!(value.state, PetState::Working);
        assert_eq!(value.sequence, 4);
        assert_eq!(value.progress.unwrap().total, 3);
    }

    #[test]
    fn rejects_unknown_states_and_invalid_progress() {
        assert!(
            parse_wire_snapshot(
                br#"{"version":1,"sequence":1,"state":"success","phase":"done","updatedAtMs":42}"#,
            )
            .is_err()
        );
        assert!(parse_wire_snapshot(
            br#"{"version":1,"sequence":1,"state":"idle","phase":"done","progress":{"completed":2,"total":1},"updatedAtMs":42}"#,
        )
        .is_err());
    }

    #[test]
    fn endpoint_is_loopback_and_token_is_not_in_the_url() {
        let endpoint = PetBridgeEndpoint::new(1234, "secret");
        assert_eq!(endpoint.listen_addr(), "127.0.0.1:1234");
        assert_eq!(
            endpoint.events_url(9),
            "http://127.0.0.1:1234/pet/events?since=9"
        );
        assert!(!endpoint.events_url(9).contains("secret"));
    }

    #[test]
    fn preference_patch_validates_catalog_ids_and_scale() {
        let mut preferences = PetPreferences::default();
        preferences
            .apply_patch(PetPreferencesPatch {
                enabled: Some(true),
                selected_pet_id: Some("marmot_2".into()),
                scale: Some(1.4),
                ..PetPreferencesPatch::default()
            })
            .unwrap();
        assert!(preferences.enabled);
        assert_eq!(preferences.selected_pet_id, "marmot_2");
        assert_eq!(preferences.scale, 1.4);

        assert_eq!(
            preferences
                .apply_patch(PetPreferencesPatch {
                    selected_pet_id: Some("../marmot".into()),
                    ..PetPreferencesPatch::default()
                })
                .unwrap_err()
                .code,
            "petSelectionInvalid"
        );
        assert_eq!(
            preferences
                .apply_patch(PetPreferencesPatch {
                    scale: Some(1.41),
                    ..PetPreferencesPatch::default()
                })
                .unwrap_err()
                .code,
            "petScaleInvalid"
        );
    }

    #[test]
    fn heartbeat_marks_a_reconnected_stream_connected_without_a_new_snapshot() {
        let snapshot = Arc::new(Mutex::new(PetSnapshot {
            bridge_status: PetBridgeStatus::Stale,
            state: PetState::Working,
            phase: "tool-call".into(),
            sequence: 7,
            updated_at_ms: Some(42),
            ..PetSnapshot::default()
        }));
        let published = Arc::new(Mutex::new(Vec::new()));
        let published_for_listener = Arc::clone(&published);
        let listener: PetListener = Arc::new(move |value| {
            published_for_listener.lock().unwrap().push(value);
        });

        read_sse_stream(
            std::io::Cursor::new(b"retry: 500\n\n: heartbeat\n\n"),
            &AtomicBool::new(false),
            &snapshot,
            &listener,
        )
        .unwrap();

        let current = snapshot.lock().unwrap().clone();
        assert_eq!(current.bridge_status, PetBridgeStatus::Connected);
        assert_eq!(current.state, PetState::Working);
        assert_eq!(current.sequence, 7);
        assert_eq!(published.lock().unwrap().as_slice(), &[current]);
    }
}
