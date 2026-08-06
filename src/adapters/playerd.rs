use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use url::Url;

use crate::config::PlayerdConfig;
use crate::features::playback::MusicPlayerBackend;
use crate::features::playback::PlayerStatus;
use crate::features::song_request::CandidateEligibility;
use crate::runtime::player::{PlayerRuntimeMetadata, RawPlayerSample, TransportState};
use crate::runtime::player_io::{
    ControlDispatchOutcome, PickedCandidate as RuntimePickedCandidate, PlayerControl,
    PlayerControlPort, PlayerObservationPort, PlayerObservationReadError, PlayerSearchError,
    PlayerSearchPort, SearchCandidate,
};

const PROVIDERS: [&str; 2] = ["qqmusic", "netease"];

#[derive(Clone, Debug)]
struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
}

#[derive(Clone, Debug)]
struct SessionState {
    session_id: String,
    generation: u64,
}

struct PlayerdShared {
    base_url: String,
    token: String,
    client: Client,
    runtime: Arc<Runtime>,
    request_timeout: Duration,
    tracks: Mutex<HashMap<String, TrackMetadata>>,
    locators: Mutex<HashMap<String, String>>,
    locator_path: PathBuf,
    session: Mutex<Option<SessionState>>,
    operation_sequence: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct PlayerdClient {
    shared: Arc<PlayerdShared>,
}

#[derive(Clone)]
struct PlayerdLaunch {
    executable: PathBuf,
    working_dir: PathBuf,
    address: String,
    token_path: PathBuf,
    request_timeout: Duration,
    startup_timeout: Duration,
}

struct SupervisorControl {
    stopping: AtomicBool,
    child: Mutex<Option<Child>>,
}

pub(crate) struct PlayerdSupervisor {
    control: Option<Arc<SupervisorControl>>,
    monitor: Option<JoinHandle<()>>,
}

impl Drop for PlayerdSupervisor {
    fn drop(&mut self) {
        let Some(control) = self.control.take() else {
            return;
        };
        control.stopping.store(true, Ordering::SeqCst);
        if let Ok(mut child) = control.child.lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.kill();
        }
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

#[derive(Debug)]
struct PlayerdHttpError {
    status: Option<StatusCode>,
    code: Option<String>,
    message: String,
    transport: bool,
}

impl std::fmt::Display for PlayerdHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = &self.code {
            write!(formatter, "playerd failure [{code}]: {}", self.message)
        } else if let Some(status) = self.status {
            write!(
                formatter,
                "playerd HTTP {}: {}",
                status.as_u16(),
                self.message
            )
        } else {
            formatter.write_str(&self.message)
        }
    }
}

impl std::error::Error for PlayerdHttpError {}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailureResponse {
    code: String,
    message: String,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    retry_after_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct ErrorResponse {
    failure: FailureResponse,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    outcomes: Vec<SearchOutcome>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchOutcome {
    provider: String,
    candidates: Vec<SearchResultSong>,
    #[serde(default)]
    failure: Option<FailureResponse>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResultSong {
    key: SongKeyResponse,
    resolver_locator: Option<String>,
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    eligibility: String,
}

#[derive(Clone, Debug, Deserialize)]
struct SongKeyResponse {
    source: String,
    id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartResponse {
    session_id: String,
    generation: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateResponse {
    session_id: Option<String>,
    generation: u64,
    state: String,
    song_key: Option<SongKeyResponse>,
    #[serde(default, alias = "runtimeId")]
    runtime_identity: Option<String>,
    #[serde(default)]
    end_behavior: Option<String>,
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    volume: u8,
    #[serde(default)]
    last_end_cause: Option<String>,
    #[serde(default)]
    failure: Option<FailureResponse>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionRequest {
    session_id: String,
    generation: u64,
    operation_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    song_key: SongKeyRequest,
    resolver_locator: Option<String>,
    end_behavior: &'static str,
    operation_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct SongKeyRequest {
    source: String,
    id: String,
}

#[derive(Clone, Debug, Serialize)]
struct VolumeRequest {
    volume: u8,
    operation_id: String,
}

impl PlayerdSupervisor {
    pub(crate) fn start(config: &PlayerdConfig) -> Result<Self> {
        let working_dir = absolute_path(&config.working_dir)?;
        let address = format_host_port(&config.host, config.port);
        let request_timeout = Duration::from_millis(config.request_timeout_ms);
        match health_probe(&address, Duration::from_millis(config.request_timeout_ms))? {
            HealthProbe::Ready => {
                return Ok(Self {
                    control: None,
                    monitor: None,
                });
            }
            HealthProbe::Unhealthy => {
                bail!("已有 miliastra-playerd 进程但 health 返回 unhealthy")
            }
            HealthProbe::Unavailable if !config.auto_start => {
                bail!("miliastra-playerd 未运行且 playerd.auto_start=false")
            }
            HealthProbe::Unavailable => {}
        }

        let executable = if config.executable.is_absolute() {
            config.executable.clone()
        } else {
            working_dir.join(&config.executable)
        };
        let launch = PlayerdLaunch {
            executable,
            working_dir,
            address,
            token_path: config.token_path.clone(),
            request_timeout,
            startup_timeout: Duration::from_millis(config.startup_timeout_ms),
        };
        let child = launch.spawn()?;
        if let Err(error) = wait_for_health(&launch) {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let control = Arc::new(SupervisorControl {
            stopping: AtomicBool::new(false),
            child: Mutex::new(Some(child)),
        });
        let monitor_control = Arc::clone(&control);
        let monitor_launch = launch.clone();
        let restart_limit = config.restart_limit;
        let monitor = std::thread::spawn(move || {
            monitor_children(monitor_control, monitor_launch, restart_limit);
        });
        Ok(Self {
            control: Some(control),
            monitor: Some(monitor),
        })
    }

    pub(crate) fn shutdown(mut self) -> Result<()> {
        let Some(control) = self.control.take() else {
            return Ok(());
        };
        control.stopping.store(true, Ordering::SeqCst);
        if let Ok(mut child) = control.child.lock()
            && let Some(child) = child.as_mut()
            && child.try_wait()?.is_none()
        {
            child.kill().context("终止 miliastra-playerd")?;
        }
        if let Some(monitor) = self.monitor.take() {
            monitor
                .join()
                .map_err(|_| anyhow!("playerd supervisor monitor thread panicked"))?;
        }
        Ok(())
    }
}

impl PlayerdLaunch {
    fn spawn(&self) -> Result<Child> {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.working_dir)
            .arg("--bind")
            .arg(&self.address)
            .arg("--token-path")
            .arg(&self.token_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .with_context(|| format!("启动 miliastra-playerd: {}", self.executable.display()))
    }
}

fn wait_for_health(launch: &PlayerdLaunch) -> Result<()> {
    let deadline = Instant::now() + launch.startup_timeout;
    loop {
        match health_probe(&launch.address, launch.request_timeout)? {
            HealthProbe::Ready => return Ok(()),
            HealthProbe::Unhealthy | HealthProbe::Unavailable if Instant::now() >= deadline => {
                bail!(
                    "miliastra-playerd 未在 {}ms 内通过 health 检查",
                    launch.startup_timeout.as_millis()
                )
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn monitor_children(control: Arc<SupervisorControl>, launch: PlayerdLaunch, restart_limit: u32) {
    let mut restarts = 0;
    loop {
        if control.stopping.load(Ordering::SeqCst) {
            return;
        }
        let exited = match control.child.lock() {
            Ok(mut child) => match child.as_mut() {
                Some(child) => child.try_wait().ok().flatten().is_some(),
                None => true,
            },
            Err(_) => true,
        };
        if !exited {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if control.stopping.load(Ordering::SeqCst) || restarts >= restart_limit {
            log::error!("miliastra-playerd 已退出，重启次数已用尽: {restarts}");
            return;
        }
        restarts += 1;
        match launch.spawn().and_then(|child| {
            if let Ok(mut slot) = control.child.lock() {
                *slot = Some(child);
            }
            wait_for_health(&launch)
        }) {
            Ok(()) => log::warn!("miliastra-playerd 已自动重启: attempt={restarts}"),
            Err(error) => {
                log::error!("miliastra-playerd 自动重启失败: {error:#}");
                if let Ok(mut child) = control.child.lock()
                    && let Some(child) = child.as_mut()
                {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }
}

enum HealthProbe {
    Ready,
    Unhealthy,
    Unavailable,
}

fn health_probe(address: &str, timeout: Duration) -> Result<HealthProbe> {
    let client = Client::builder().timeout(timeout).build()?;
    let runtime = Runtime::new().context("创建 playerd health runtime")?;
    let url = format!("http://{address}/health");
    let result = runtime.block_on(async { client.get(url).send().await });
    match result {
        Ok(response) if response.status() == StatusCode::OK => Ok(HealthProbe::Ready),
        Ok(response) if response.status() == StatusCode::SERVICE_UNAVAILABLE => {
            Ok(HealthProbe::Unhealthy)
        }
        Ok(_) | Err(_) => Ok(HealthProbe::Unavailable),
    }
}

impl PlayerdClient {
    pub(crate) fn connect(config: &PlayerdConfig) -> Result<Self> {
        let working_dir = absolute_path(&config.working_dir)?;
        let token_path = resolve_path(&working_dir, &config.token_path);
        let token = fs::read_to_string(&token_path)
            .with_context(|| format!("读取 playerd API token: {}", token_path.display()))?;
        let token = token.trim().to_owned();
        if token.is_empty() {
            bail!("playerd API token 为空: {}", token_path.display());
        }
        let locator_path = resolve_path(&working_dir, &config.resolver_locator_path);
        let locators = load_locators(&locator_path)?;
        let runtime = Arc::new(Runtime::new().context("创建 playerd HTTP runtime")?);
        let client = Client::builder()
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("创建 playerd HTTP client")?;
        Ok(Self {
            shared: Arc::new(PlayerdShared {
                base_url: format!("http://{}", format_host_port(&config.host, config.port)),
                token,
                client,
                runtime,
                request_timeout: Duration::from_millis(config.request_timeout_ms),
                tracks: Mutex::new(HashMap::new()),
                locators: Mutex::new(locators),
                locator_path,
                session: Mutex::new(None),
                operation_sequence: AtomicU64::new(1),
            }),
        })
    }

    fn operation_id(&self, kind: &str) -> String {
        let number = self
            .shared
            .operation_sequence
            .fetch_add(1, Ordering::Relaxed);
        format!("mainline-{kind}-{number}")
    }

    fn request_builder(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.shared
            .client
            .request(method, format!("{}{path}", self.shared.base_url))
            .bearer_auth(&self.shared.token)
            .timeout(self.shared.request_timeout)
    }

    fn perform(&self, request: RequestBuilder) -> std::result::Result<String, PlayerdHttpError> {
        let runtime = Arc::clone(&self.shared.runtime);
        let result = runtime.block_on(async {
            let response = request.send().await.map_err(|error| PlayerdHttpError {
                status: None,
                code: None,
                message: error.to_string(),
                transport: true,
            })?;
            let status = response.status();
            let body = response.text().await.map_err(|error| PlayerdHttpError {
                status: Some(status),
                code: None,
                message: error.to_string(),
                transport: false,
            })?;
            Ok::<_, PlayerdHttpError>((status, body))
        });
        let (status, body) = result?;
        if status.is_success() {
            return Ok(body);
        }
        let parsed = serde_json::from_str::<ErrorResponse>(&body).ok();
        Err(PlayerdHttpError {
            status: Some(status),
            code: parsed.as_ref().map(|body| body.failure.code.clone()),
            message: parsed
                .map(|body| body.failure.message)
                .unwrap_or_else(|| body.trim().to_string()),
            transport: false,
        })
    }

    fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> std::result::Result<T, PlayerdHttpError> {
        let request = self
            .request_builder(reqwest::Method::GET, path)
            .query(query);
        let body = self.perform(request)?;
        serde_json::from_str(&body).map_err(|error| PlayerdHttpError {
            status: None,
            code: Some("invalid_playerd_response".to_string()),
            message: error.to_string(),
            transport: false,
        })
    }

    fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> std::result::Result<T, PlayerdHttpError> {
        let request = self.request_builder(reqwest::Method::POST, path).json(body);
        let body = self.perform(request)?;
        serde_json::from_str(&body).map_err(|error| PlayerdHttpError {
            status: None,
            code: Some("invalid_playerd_response".to_string()),
            message: error.to_string(),
            transport: false,
        })
    }

    fn post_empty<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> std::result::Result<(), PlayerdHttpError> {
        let request = self.request_builder(reqwest::Method::POST, path).json(body);
        self.perform(request).map(|_| ())
    }

    fn state(&self) -> std::result::Result<StateResponse, PlayerdHttpError> {
        self.get_json("/v1/state", &[])
    }

    fn current_session(&self) -> std::result::Result<SessionState, String> {
        self.shared
            .session
            .lock()
            .map_err(|_| "playerd session state lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "playerd has no active playback session".to_string())
    }

    fn control_session(&self, kind: &str) -> std::result::Result<SessionRequest, String> {
        let session = self.current_session()?;
        Ok(SessionRequest {
            session_id: session.session_id,
            generation: session.generation,
            operation_id: self.operation_id(kind),
        })
    }

    fn save_track_locator(&self, uri: &str, locator: Option<&str>) {
        let Some(locator) = locator.filter(|locator| !locator.trim().is_empty()) else {
            return;
        };
        let Ok(mut locators) = self.shared.locators.lock() else {
            log::warn!("playerd resolver locator lock poisoned");
            return;
        };
        locators.insert(uri.to_string(), locator.to_string());
        match serde_json::to_vec_pretty(&*locators) {
            Ok(bytes) => {
                if let Some(parent) = self.shared.locator_path.parent()
                    && let Err(error) = fs::create_dir_all(parent)
                {
                    log::warn!("创建 resolver locator 目录失败: {error}");
                    return;
                }
                if let Err(error) = fs::write(&self.shared.locator_path, bytes) {
                    log::warn!("保存 resolver locator 失败: {error}");
                }
            }
            Err(error) => log::warn!("序列化 resolver locator 失败: {error}"),
        }
    }

    fn locator_for(&self, uri: &str) -> Option<String> {
        self.shared
            .locators
            .lock()
            .ok()
            .and_then(|locators| locators.get(uri).cloned())
    }

    fn remember_track(&self, uri: &str, song: &SearchResultSong) {
        let artist = song.artists.join("/");
        if let Ok(mut tracks) = self.shared.tracks.lock() {
            tracks.insert(
                uri.to_string(),
                TrackMetadata {
                    title: song.title.clone(),
                    artist,
                    album: song.album.clone().unwrap_or_default(),
                },
            );
        }
        self.save_track_locator(uri, song.resolver_locator.as_deref());
    }

    fn search_internal(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, PlayerdHttpError> {
        let mut query = vec![("keyword".to_string(), keyword.trim().to_string())];
        let providers = normalize_providers(source)?;
        if let Some(providers) = providers {
            query.push(("providers".to_string(), providers));
        }
        query.push(("limit".to_string(), "10".to_string()));
        let response: SearchResponse = self.get_json("/v1/search", &query)?;
        let mut candidates = Vec::new();
        let mut retained_by_provider = HashMap::<String, usize>::new();
        for outcome in response.outcomes {
            if let Some(failure) = outcome.failure {
                log::warn!(
                    "playerd 搜索 provider={} 失败 code={} retryable={}",
                    outcome.provider,
                    failure.code,
                    failure.retryable
                );
            }
            for song in outcome.candidates {
                if retained_by_provider
                    .get(&outcome.provider)
                    .copied()
                    .unwrap_or_default()
                    >= 5
                {
                    continue;
                }
                let Some(candidate) = search_candidate_from_playerd(&outcome.provider, &song)?
                else {
                    continue;
                };
                self.remember_track(&candidate.uri, &song);
                candidates.push(candidate);
                *retained_by_provider
                    .entry(outcome.provider.clone())
                    .or_default() += 1;
            }
        }
        Ok(candidates)
    }

    fn search_candidates_result(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, PlayerSearchError> {
        self.search_internal(keyword, source)
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }

    fn dispatch_http(&self, control: &PlayerControl) -> ControlDispatchOutcome {
        let result = match control {
            PlayerControl::PlayUri(uri) => self.start_uri(uri),
            PlayerControl::Pause => {
                self.control_session("pause")
                    .map_err(local_error)
                    .and_then(|request| {
                        self.post_empty("/v1/control/pause", &request)
                            .map_err(http_error)
                    })
            }
            PlayerControl::Resume => self
                .control_session("resume")
                .map_err(local_error)
                .and_then(|request| {
                    self.post_empty("/v1/control/resume", &request)
                        .map_err(http_error)
                }),
            PlayerControl::Next | PlayerControl::Previous => {
                return ControlDispatchOutcome::rejected(
                    "playerd 不拥有上一首/下一首；由主线队列决定下一首",
                );
            }
            PlayerControl::SetVolume(volume) => {
                if *volume > 100 {
                    return ControlDispatchOutcome::not_sent("volume 参数必须是 0-100");
                }
                self.post_empty(
                    "/v1/control/volume",
                    &VolumeRequest {
                        volume: *volume,
                        operation_id: self.operation_id("volume"),
                    },
                )
                .map_err(http_error)
            }
        };

        match result {
            Ok(_) => ControlDispatchOutcome::acknowledged("playerd accepted"),
            Err(error) if error.transport => {
                ControlDispatchOutcome::outcome_unknown(error.to_string())
            }
            Err(error) => match error.code.clone() {
                Some(code) => ControlDispatchOutcome::rejected_with_code(error.to_string(), code),
                None => ControlDispatchOutcome::rejected(error.to_string()),
            },
        }
    }

    fn start_uri(&self, uri: &str) -> std::result::Result<(), PlayerdHttpError> {
        let (source, id) = parse_track_uri(uri).map_err(|error| local_error(error.to_string()))?;
        let request = StartRequest {
            song_key: SongKeyRequest { source, id },
            resolver_locator: self.locator_for(uri),
            end_behavior: "notify_controller",
            operation_id: self.operation_id("start"),
        };
        let receipt: StartResponse = self.post_json("/v1/sessions", &request)?;
        let mut session = self
            .shared
            .session
            .lock()
            .map_err(|_| local_error("playerd session state lock poisoned"))?;
        *session = Some(SessionState {
            session_id: receipt.session_id,
            generation: receipt.generation,
        });
        Ok(())
    }

    fn observe_state(&self, state: &StateResponse) {
        if let Some(session_id) = state.session_id.clone()
            && let Ok(mut session) = self.shared.session.lock()
        {
            *session = Some(SessionState {
                session_id,
                generation: state.generation,
            });
        }
        if let Some(failure) = state.failure.as_ref() {
            log::debug!(
                "playerd state failure code={} retryable={}",
                failure.code,
                failure.retryable
            );
        }
    }

    fn raw_sample_from_state(
        &self,
        state: &StateResponse,
    ) -> std::result::Result<RawPlayerSample, PlayerdHttpError> {
        let uri = state
            .song_key
            .as_ref()
            .map(|key| track_uri(&key.source, &key.id))
            .transpose()
            .map_err(|error| PlayerdHttpError {
                status: None,
                code: Some("invalid_playerd_response".to_string()),
                message: error.to_string(),
                transport: false,
            })?;
        let metadata = uri.as_deref().and_then(|uri| {
            self.shared
                .tracks
                .lock()
                .ok()
                .and_then(|tracks| tracks.get(uri).cloned())
        });
        let transport = match state.state.as_str() {
            "playing" => Some(TransportState::Playing),
            "paused" => Some(TransportState::Paused),
            "stopped" | "failed" | "idle" => Some(TransportState::Stopped),
            _ => None,
        };
        Ok(RawPlayerSample {
            uri,
            transport,
            title: metadata.as_ref().map(|metadata| metadata.title.clone()),
            artist: metadata.as_ref().map(|metadata| metadata.artist.clone()),
            album_name: metadata.as_ref().map(|metadata| metadata.album.clone()),
            lyric_line_text: None,
            progress: finite_duration(state.position_seconds),
            duration: finite_duration(state.duration_seconds),
            playback_rate: Some(1.0),
            volume: Some(i64::from(state.volume)),
            runtime: PlayerRuntimeMetadata {
                runtime_identity: state.runtime_identity.clone().unwrap_or_default(),
                session_id: state.session_id.clone().unwrap_or_default(),
                generation: state.generation,
                end_behavior: state.end_behavior.clone().unwrap_or_default(),
                last_end_cause: state.last_end_cause.clone().unwrap_or_default(),
                failure_code: state
                    .failure
                    .as_ref()
                    .map(|failure| failure.code.clone())
                    .unwrap_or_default(),
                failure_message: state
                    .failure
                    .as_ref()
                    .map(|failure| failure.message.clone())
                    .unwrap_or_default(),
                failure_retryable: state
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.retryable),
                failure_provider: state
                    .failure
                    .as_ref()
                    .and_then(|failure| failure.provider.clone())
                    .unwrap_or_default(),
                failure_retry_after_ms: state
                    .failure
                    .as_ref()
                    .and_then(|failure| failure.retry_after_ms)
                    .unwrap_or_default(),
            },
        })
    }

    fn raw_sample(&self) -> std::result::Result<RawPlayerSample, PlayerdHttpError> {
        let state = self.state()?;
        self.observe_state(&state);
        self.raw_sample_from_state(&state)
    }

    fn player_status(&self) -> Result<PlayerStatus> {
        let state = self.state().map_err(|error| anyhow!(error.to_string()))?;
        self.observe_state(&state);
        let sample = self
            .raw_sample_from_state(&state)
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(player_status_from_state(&state, sample))
    }
}

impl PlayerControlPort for PlayerdClient {
    fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatchOutcome {
        self.dispatch_http(control)
    }
}

impl PlayerObservationPort for PlayerdClient {
    fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
        self.raw_sample()
            .map_err(|error| PlayerObservationReadError::new(error.to_string()))
    }
}

impl PlayerSearchPort for PlayerdClient {
    fn search_text(&mut self, keyword: &str, source: &str) -> Result<String, PlayerSearchError> {
        let candidates = self.search_candidates_result(keyword, source)?;
        Ok(format_candidates(&candidates))
    }

    fn search_candidates(
        &mut self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
        self.search_candidates_result(keyword, source)
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }

    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<RuntimePickedCandidate>, PlayerSearchError> {
        let mut searches = Vec::new();
        if prefer_accompaniment && !is_accompaniment(keyword) {
            searches.push(format!("{keyword} 伴奏"));
        }
        searches.push(keyword.to_string());
        let mut fallback = None;
        for search in searches {
            // The playerd daemon searches providers concurrently. Keep every
            // returned source in the immutable request snapshot while choosing
            // the initial candidate from the command's requested provider.
            let candidate_snapshot = self.search_candidates_result(&search, "all")?;
            let candidates = candidates_for_source(&candidate_snapshot, source);
            let formatted = format_candidates(&candidates);
            let Some(candidate) = pick_candidate(&candidates, prefer_accompaniment) else {
                continue;
            };
            if !prefer_accompaniment || is_accompaniment(&candidate.text) {
                return Ok(Some(RuntimePickedCandidate::with_snapshot(
                    candidate,
                    candidate_snapshot,
                    formatted,
                )));
            }
            fallback = Some(RuntimePickedCandidate::with_snapshot(
                candidate,
                candidate_snapshot,
                formatted,
            ));
        }
        Ok(fallback)
    }
}

impl MusicPlayerBackend for PlayerdClient {
    fn status(&self) -> Result<PlayerStatus> {
        self.player_status()
    }

    fn play_uri(&self, uri: &str) -> Result<String> {
        self.start_uri(uri).map_err(anyhow::Error::new)?;
        Ok("playerd accepted".to_string())
    }

    fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
        is_track_unavailable_error(error)
    }

    fn pause(&self) -> Result<String> {
        let request = self.control_session("pause").map_err(anyhow::Error::msg)?;
        self.post_empty("/v1/control/pause", &request)
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok("playerd accepted".to_string())
    }

    fn resume(&self) -> Result<String> {
        let request = self.control_session("resume").map_err(anyhow::Error::msg)?;
        self.post_empty("/v1/control/resume", &request)
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok("playerd accepted".to_string())
    }

    fn next(&self) -> Result<String> {
        bail!("playerd 不拥有下一首；由主线队列决定下一首")
    }

    fn previous(&self) -> Result<String> {
        bail!("playerd 不拥有上一首；由主线队列决定上一首")
    }

    fn set_volume(&self, volume: &str) -> Result<String> {
        let volume = volume.parse::<u8>().context("volume 参数必须是 0-100")?;
        if volume > 100 {
            bail!("volume 参数必须是 0-100");
        }
        self.post_empty(
            "/v1/control/volume",
            &VolumeRequest {
                volume,
                operation_id: self.operation_id("volume"),
            },
        )
        .map_err(|error| anyhow!(error.to_string()))?;
        Ok("playerd accepted".to_string())
    }
}

fn http_error(error: PlayerdHttpError) -> PlayerdHttpError {
    error
}

fn local_error(message: impl Into<String>) -> PlayerdHttpError {
    PlayerdHttpError {
        status: None,
        code: Some("invalid_local_request".to_string()),
        message: message.into(),
        transport: false,
    }
}

fn is_track_unavailable_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<PlayerdHttpError>()
        .and_then(|error| error.code.as_deref())
        == Some("track_unavailable")
}

fn normalize_providers(source: &str) -> std::result::Result<Option<String>, PlayerdHttpError> {
    let source = source.trim();
    if source.is_empty() || source.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let mut providers = Vec::new();
    for provider in source
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !PROVIDERS.contains(&provider) {
            return Err(local_error(format!(
                "不支持的 playerd provider: {provider}"
            )));
        }
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    if providers.is_empty() {
        Ok(None)
    } else {
        Ok(Some(providers.join(",")))
    }
}

fn track_uri(source: &str, id: &str) -> Result<String> {
    if source.is_empty() || id.is_empty() || source.contains('/') || id.contains('/') {
        bail!("playerd song key contains an invalid source or id");
    }
    Ok(format!("miliastra://track/{source}/{id}"))
}

fn parse_track_uri(uri: &str) -> Result<(String, String)> {
    let parsed = Url::parse(uri).with_context(|| format!("解析 playerd URI 失败: {uri}"))?;
    if parsed.scheme() != "miliastra" || parsed.host_str() != Some("track") {
        bail!("只允许 miliastra://track/<source>/<id> URI");
    }
    let parts = parsed
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    if parts.len() != 2 || parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("playerd URI 必须是 miliastra://track/<source>/<id>");
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn format_candidate_text(song: &SearchResultSong) -> String {
    let artists = if song.artists.is_empty() {
        "未知歌手".to_string()
    } else {
        song.artists.join("/")
    };
    let album = song
        .album
        .as_deref()
        .filter(|album| !album.trim().is_empty())
        .map(|album| format!(" / {album}"))
        .unwrap_or_default();
    format!(
        "{} - {}{} [{}]",
        song.title, artists, album, song.key.source
    )
}

fn search_candidate_from_playerd(
    provider: &str,
    song: &SearchResultSong,
) -> std::result::Result<Option<SearchCandidate>, PlayerdHttpError> {
    let eligibility = CandidateEligibility::from_playerd(&song.eligibility);
    if eligibility == CandidateEligibility::Ineligible || song.key.source != provider {
        return Ok(None);
    }
    let uri = track_uri(&song.key.source, &song.key.id).map_err(|error| PlayerdHttpError {
        status: None,
        code: Some("invalid_playerd_response".to_string()),
        message: error.to_string(),
        transport: false,
    })?;
    Ok(Some(SearchCandidate::with_metadata(
        format_candidate_text(song),
        uri,
        eligibility,
        song.resolver_locator.clone(),
    )))
}

fn format_candidates(candidates: &[SearchCandidate]) -> String {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| format!("{}. {}", index + 1, candidate.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_accompaniment(text: &str) -> bool {
    ["伴奏", "纯音乐", "instrumental", "karaoke", "off vocal"]
        .iter()
        .any(|word| text.to_ascii_lowercase().contains(word))
}

fn pick_candidate(
    candidates: &[SearchCandidate],
    prefer_accompaniment: bool,
) -> Option<SearchCandidate> {
    let accompaniment = candidates
        .iter()
        .filter(|candidate| is_accompaniment(&candidate.text))
        .cloned()
        .collect::<Vec<_>>();
    let comparable = if prefer_accompaniment && !accompaniment.is_empty() {
        accompaniment.as_slice()
    } else {
        candidates
    };
    SearchCandidate::select_preferred_equivalent(comparable)
}

fn candidates_for_source(candidates: &[SearchCandidate], source: &str) -> Vec<SearchCandidate> {
    let source = source.trim();
    if source.is_empty() || source.eq_ignore_ascii_case("all") {
        return candidates.to_vec();
    }
    candidates
        .iter()
        .filter(|candidate| {
            parse_track_uri(&candidate.uri)
                .map(|(candidate_source, _)| candidate_source == source)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn finite_duration(value: Option<f64>) -> Option<Duration> {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .and_then(|value| Duration::try_from_secs_f64(value).ok())
}

fn player_status_from_state(state: &StateResponse, sample: RawPlayerSample) -> PlayerStatus {
    let failure = state.failure.as_ref();
    PlayerStatus {
        status: match sample.transport {
            Some(TransportState::Playing) => "playing",
            Some(TransportState::Paused) => "paused",
            _ => "stopped",
        }
        .to_string(),
        current_uri: sample.uri.unwrap_or_default(),
        name: sample.title.unwrap_or_default(),
        singer: sample.artist.unwrap_or_default(),
        album_name: sample.album_name.unwrap_or_default(),
        lyric_line_text: String::new(),
        duration: sample
            .duration
            .map_or(0.0, |duration| duration.as_secs_f64()),
        progress: sample
            .progress
            .map_or(0.0, |progress| progress.as_secs_f64()),
        playback_rate: 1.0,
        volume: sample.volume.unwrap_or_default(),
        requester: String::new(),
        runtime_identity: state.runtime_identity.clone().unwrap_or_default(),
        session_id: state.session_id.clone().unwrap_or_default(),
        generation: state.generation,
        end_behavior: state.end_behavior.clone().unwrap_or_default(),
        last_end_cause: state.last_end_cause.clone().unwrap_or_default(),
        failure_code: failure
            .map(|failure| failure.code.clone())
            .unwrap_or_default(),
        failure_message: failure
            .map(|failure| failure.message.clone())
            .unwrap_or_default(),
        failure_retryable: failure.is_some_and(|failure| failure.retryable),
        failure_provider: failure
            .and_then(|failure| failure.provider.clone())
            .unwrap_or_default(),
        failure_retry_after_ms: failure
            .and_then(|failure| failure.retry_after_ms)
            .unwrap_or_default(),
    }
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn load_locators(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取 resolver locator 文件: {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("解析 resolver locator 文件: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::Error;
    use reqwest::StatusCode;
    use serde_json::json;

    use super::{
        PlayerdHttpError, RawPlayerSample, SearchResultSong, SongKeyResponse, StateResponse,
        TransportState, candidates_for_source, format_host_port, is_track_unavailable_error,
        parse_track_uri, player_status_from_state, search_candidate_from_playerd, track_uri,
    };
    use crate::features::song_request::{CandidateEligibility, SearchCandidate};

    #[test]
    fn only_the_track_unavailable_code_requests_a_source_switch() {
        let unavailable = Error::new(PlayerdHttpError {
            status: Some(StatusCode::BAD_REQUEST),
            code: Some("track_unavailable".to_string()),
            message: "当前歌曲不可播放".to_string(),
            transport: false,
        });
        let authentication = Error::new(PlayerdHttpError {
            status: Some(StatusCode::UNAUTHORIZED),
            code: Some("authentication_failed".to_string()),
            message: "token invalid".to_string(),
            transport: false,
        });
        let transport = Error::new(PlayerdHttpError {
            status: None,
            code: None,
            message: "connection refused".to_string(),
            transport: true,
        });

        assert!(is_track_unavailable_error(&unavailable));
        assert!(!is_track_unavailable_error(&authentication));
        assert!(!is_track_unavailable_error(&transport));
    }

    #[test]
    fn canonical_track_uri_round_trips() {
        let uri = track_uri("qqmusic", "0039MnYb0qxYhV").unwrap();
        assert_eq!(uri, "miliastra://track/qqmusic/0039MnYb0qxYhV");
        assert_eq!(
            parse_track_uri(&uri).unwrap(),
            ("qqmusic".into(), "0039MnYb0qxYhV".into())
        );
    }

    #[test]
    fn canonical_track_uri_rejects_legacy_or_locator_query() {
        assert!(parse_track_uri("fuo://qqmusic/songs/1").is_err());
        assert!(parse_track_uri("miliastra://track/qqmusic/1?locator=x").is_err());
    }

    #[test]
    fn initial_snapshot_can_include_both_providers_while_selection_stays_on_requested_source() {
        let candidates = vec![
            SearchCandidate::new("QQ song", "miliastra://track/qqmusic/1"),
            SearchCandidate::new("NetEase song", "miliastra://track/netease/2"),
        ];

        assert_eq!(
            candidates_for_source(&candidates, "qqmusic"),
            vec![candidates[0].clone()]
        );
        assert_eq!(candidates_for_source(&candidates, "all"), candidates);
    }

    #[test]
    fn playerd_search_candidate_retains_rights_and_resolver_locator() {
        let song = SearchResultSong {
            key: SongKeyResponse {
                source: "netease".to_string(),
                id: "5185366".to_string(),
            },
            resolver_locator: Some("netease:5185366".to_string()),
            title: "Canon".to_string(),
            artists: vec!["Pachelbel".to_string()],
            album: None,
            eligibility: "eligible".to_string(),
        };

        let candidate = search_candidate_from_playerd("netease", &song)
            .expect("valid playerd candidate")
            .expect("eligible candidate");

        assert_eq!(candidate.eligibility, CandidateEligibility::Eligible);
        assert_eq!(
            candidate.resolver_locator.as_deref(),
            Some("netease:5185366")
        );
        assert_eq!(candidate.uri, "miliastra://track/netease/5185366");
    }

    #[test]
    fn host_port_formats_ipv6_without_changing_normal_hosts() {
        assert_eq!(format_host_port("127.0.0.1", 17854), "127.0.0.1:17854");
        assert_eq!(format_host_port("::1", 17854), "[::1]:17854");
    }

    #[test]
    fn state_response_preserves_durable_terminal_metadata() {
        let state: StateResponse = serde_json::from_value(json!({
            "sessionId": "session-1",
            "generation": 8,
            "state": "stopped",
            "songKey": { "source": "netease", "id": "5185366" },
            "runtimeIdentity": "runtime-1",
            "endBehavior": "notify_controller",
            "positionSeconds": 180.0,
            "durationSeconds": 180.0,
            "volume": 70,
            "lastEndCause": "natural_end",
            "failure": {
                "code": "provider_timeout",
                "message": "timed out",
                "retryable": true,
                "provider": "netease",
                "retryAfterMs": 500
            }
        }))
        .unwrap();

        assert_eq!(state.runtime_identity.as_deref(), Some("runtime-1"));
        assert_eq!(state.end_behavior.as_deref(), Some("notify_controller"));
        assert_eq!(state.last_end_cause.as_deref(), Some("natural_end"));
        let status = player_status_from_state(
            &state,
            RawPlayerSample {
                uri: Some("miliastra://track/netease/5185366".to_string()),
                transport: Some(TransportState::Stopped),
                title: Some("Canon".to_string()),
                artist: Some("Pachelbel".to_string()),
                album_name: None,
                lyric_line_text: None,
                progress: Some(Duration::from_secs(180)),
                duration: Some(Duration::from_secs(180)),
                playback_rate: Some(1.0),
                volume: Some(70),
                ..RawPlayerSample::default()
            },
        );
        assert_eq!(status.session_id, "session-1");
        assert_eq!(status.generation, 8);
        assert_eq!(status.runtime_identity, "runtime-1");
        assert_eq!(status.end_behavior, "notify_controller");
        assert_eq!(status.last_end_cause, "natural_end");
        assert_eq!(status.failure_code, "provider_timeout");
        assert_eq!(status.failure_provider, "netease");
        assert_eq!(status.failure_retry_after_ms, 500);
        let failure = state.failure.unwrap();
        assert_eq!(failure.code, "provider_timeout");
        assert_eq!(failure.provider.as_deref(), Some("netease"));
        assert_eq!(failure.retry_after_ms, Some(500));
    }

    #[test]
    fn state_response_accepts_older_playerd_without_new_metadata() {
        let state: StateResponse = serde_json::from_value(json!({
            "sessionId": null,
            "generation": 0,
            "state": "idle",
            "songKey": null,
            "positionSeconds": null,
            "durationSeconds": null,
            "volume": 100
        }))
        .unwrap();

        assert!(state.runtime_identity.is_none());
        assert!(state.end_behavior.is_none());
        assert!(state.last_end_cause.is_none());
        assert!(state.failure.is_none());
    }
}
