use std::sync::Arc;

use anyhow::Result;
use miliastra_playback::{
    AudioCacheStats, AudioCacheTrackStatus, CachedTrackPage, CredentialStatus, LoginSession,
    PlayableTrack, PlaybackHandle, ProviderId, TrackKey,
};
use serde_json::json;
use uuid::Uuid;

use crate::adapters::login_helper::{
    LoginHelperFailure, LoginHelperManager, LoginManagerStatus, ProviderView,
};
use crate::adapters::player::PlayerRuntimeBackend;
use crate::features::administration::{
    AdministrationCommand, AdministrationMutationIntent, AdministrationMutationOutcome,
};
use crate::features::command::ModuleCommand;
use crate::features::custom_workflow::CustomWorkflowService;
use crate::features::hall::HallCommand;
use crate::features::playback::{
    MusicPlayerBackend, PlaybackCommand, PlaybackMutationIntent, PlaybackMutationOutcome,
    PlayerStatus, QueueItem,
};
use crate::features::song_request::{AiClient, SongCommand, SongSource};
use crate::features::undercover::UndercoverCommand;
use crate::interfaces::chat::ConsoleCommandIntent;
use crate::interfaces::http::{
    HttpAiPort, HttpCommandError, HttpCommandPort, HttpLoginError, HttpLoginErrorView,
    HttpLoginPort, HttpLoginStatus, HttpPlayerPort, HttpPlayerSearchError, HttpProviderView,
    HttpTaskPort, PlayTrackRequest,
};
use crate::runtime::business::{BusinessMutationIntent, BusinessMutationOutcome};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::player_io::{PlayerSearchClient, PlayerSearchClientError, SearchCandidate};
use crate::runtime::scheduler::FormalTaskEnqueueOutcome;

#[derive(Clone)]
pub(crate) struct ApplicationHttpCommandFacade {
    tasks: Arc<dyn HttpTaskPort>,
    custom_workflow: CustomWorkflowService,
}

impl ApplicationHttpCommandFacade {
    pub(crate) fn new(
        tasks: Arc<dyn HttpTaskPort>,
        custom_workflow: CustomWorkflowService,
    ) -> Self {
        Self {
            tasks,
            custom_workflow,
        }
    }

    pub(crate) fn remote_control_command(
        raw: String,
        matched: &str,
        command: ModuleCommand,
    ) -> ConsoleCommandIntent {
        ConsoleCommandIntent::new(raw, matched, command)
    }

    fn enqueue_remote_command(
        &self,
        intent: ConsoleCommandIntent,
    ) -> Result<String, HttpCommandError> {
        let pending = intent.into_pending();
        let command = pending.routed.raw.clone();
        let outcome = self
            .tasks
            .enqueue_command(pending)
            .map_err(internal_command_error)?;
        let receipt = match outcome {
            FormalTaskEnqueueOutcome::Queued(receipt) => Some(receipt),
            FormalTaskEnqueueOutcome::Duplicate => None,
        };
        Ok(json!({
            "ok": true,
            "queued": receipt.is_some(),
            "duplicate": receipt.is_none(),
            "taskId": receipt.as_ref().map(|receipt| receipt.task_id),
            "position": receipt.as_ref().map_or(0, |receipt| receipt.position),
            "command": command,
        })
        .to_string())
    }

    fn required_receipt(
        outcome: Result<FormalTaskEnqueueOutcome>,
    ) -> Result<crate::runtime::scheduler::FormalTaskReceipt, HttpCommandError> {
        match outcome.map_err(internal_command_error)? {
            FormalTaskEnqueueOutcome::Queued(receipt) => Ok(receipt),
            FormalTaskEnqueueOutcome::Duplicate => Err(HttpCommandError {
                status: 409,
                message: "任务已在待执行范围内".to_string(),
            }),
        }
    }
}

impl HttpCommandPort for ApplicationHttpCommandFacade {
    fn playback_control(&self, command: PlaybackCommand) -> Result<String, HttpCommandError> {
        let (raw, matched) = match &command {
            PlaybackCommand::Resume => ("继续".to_string(), "继续"),
            PlaybackCommand::Pause => ("暂停".to_string(), "暂停"),
            PlaybackCommand::Next => ("下一首".to_string(), "下一首"),
            PlaybackCommand::Previous => ("上一首".to_string(), "上一首"),
            PlaybackCommand::Volume(volume) => (format!("音量 {volume}"), "音量"),
            PlaybackCommand::Lyrics => ("歌词".to_string(), "歌词"),
            _ => return Err(HttpCommandError::internal("HTTP 不支持该播放控制命令")),
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw,
            matched,
            ModuleCommand::Playback(command),
        ))
    }

    fn hall_control(&self, command: HallCommand) -> Result<String, HttpCommandError> {
        let (raw, matched) = match &command {
            HallCommand::Detect => ("大厅检测".to_string(), "大厅检测"),
            HallCommand::Time => ("大厅时间".to_string(), "大厅时间"),
            HallCommand::ToggleMicrophone { .. } => ("麦克风".to_string(), "麦克风"),
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw,
            matched,
            ModuleCommand::Hall(command),
        ))
    }

    fn undercover_control(&self, start: bool) -> Result<String, HttpCommandError> {
        let (raw, command) = if start {
            ("卧底开局", UndercoverCommand::Start)
        } else {
            ("卧底结束", UndercoverCommand::End)
        };
        self.enqueue_remote_command(Self::remote_control_command(
            raw.to_string(),
            "卧底",
            ModuleCommand::Undercover(command),
        ))
    }

    fn remote_song(
        &self,
        keyword: String,
        source: String,
        prefer_accompaniment: bool,
        ai_assisted: bool,
    ) -> Result<String, HttpCommandError> {
        let contains_accompaniment = keyword.contains("伴奏");
        let keyword = keyword
            .replace("伴奏", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if keyword.trim().is_empty() {
            return Err(HttpCommandError::bad_request("缺少keyword参数"));
        }
        let prefer_accompaniment = prefer_accompaniment || contains_accompaniment;
        let (prefix, song_source) = if ai_assisted {
            ("AI点歌", SongSource::All)
        } else {
            match source.as_str() {
                "qqmusic" => ("点歌", SongSource::QqMusic),
                "netease" => ("网易点歌", SongSource::Netease),
                "bilibili" => ("B站点歌", SongSource::Bilibili),
                "kugou" => ("酷狗点歌", SongSource::Kugou),
                _ => {
                    return Err(HttpCommandError::bad_request(
                        "远程点歌source只允许qqmusic、netease、bilibili或kugou",
                    ));
                }
            }
        };
        let raw = if prefer_accompaniment {
            format!("{prefix} {keyword} 伴奏")
        } else {
            format!("{prefix} {keyword}")
        };
        self.enqueue_remote_command(ConsoleCommandIntent::new(
            raw,
            prefix,
            ModuleCommand::SongRequest(SongCommand {
                keyword,
                source: song_source,
                prefix: prefix.to_string(),
                prefer_accompaniment,
                ai_assisted,
                friend_username: String::new(),
            }),
        ))
    }

    fn play_track(&self, request: PlayTrackRequest) -> Result<String, HttpCommandError> {
        let requester = if request.requester.is_empty() {
            "WEB/API".to_owned()
        } else {
            request.requester
        };
        let track = PlayableTrack {
            track_ref: request.track_ref,
            metadata: request.metadata,
        };
        validate_playable_track(&track)?;
        let current_uri = track.track_ref.key.to_string();
        let item = QueueItem {
            id: 0,
            keyword: track.metadata.title.clone(),
            source: track.track_ref.key.provider.to_string(),
            prefer_accompaniment: false,
            ai_original_text: String::new(),
            track: Some(track.clone()),
            friend_username: String::new(),
            requester,
            // 与聊天路径一致：HTTP 默认不绕过去重；协议无权限/参数，不提供显式 bypass。
            dedup_bypass: false,
            candidate_snapshot: Vec::new(),
        };
        // 入队前复用播放队列统一去重策略（结构化/待解析交叉形态同样生效）。
        if self
            .tasks
            .playback_queue_contains(item.clone())
            .map_err(internal_command_error)?
        {
            return Err(HttpCommandError {
                status: 409,
                message: format!("队列已有: {}", track.metadata.title),
            });
        }
        let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(pushed)) = self
            .tasks
            .apply_mutation(BusinessMutationIntent::Playback(
                PlaybackMutationIntent::Push(Box::new(item)),
            ))
            .map_err(internal_command_error)?
        else {
            unreachable!("playback queue push intent returned a different outcome")
        };
        if !pushed.accepted {
            return Err(HttpCommandError::bad_request("音乐播放队列已满"));
        }
        Ok(json!({
            "ok": true,
            "queued": true,
            "size": pushed.size,
            "currentUri": current_uri,
            "track": track,
        })
        .to_string())
    }

    fn request_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<String, HttpCommandError> {
        let BusinessMutationOutcome::Administration(
            AdministrationMutationOutcome::ChatListenerModeRequested { queued, snapshot },
        ) = self
            .tasks
            .apply_mutation(BusinessMutationIntent::Administration(
                AdministrationMutationIntent::RequestChatListenerMode(mode),
            ))
            .map_err(internal_command_error)?
        else {
            unreachable!("chat listener mode intent returned a different outcome")
        };
        if !queued {
            return Ok(json!({
                "ok": true,
                "queued": false,
                "mode": snapshot.mode,
                "pendingMode": snapshot.pending_mode,
            })
            .to_string());
        }
        let receipt = match Self::required_receipt(self.tasks.enqueue_listener_mode(mode)) {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = self
                    .tasks
                    .apply_mutation(BusinessMutationIntent::Administration(
                        AdministrationMutationIntent::CancelChatListenerModeRequest(mode),
                    ));
                return Err(error);
            }
        };
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
            "mode": snapshot.mode,
            "pendingMode": mode,
        })
        .to_string())
    }

    fn set_operator_commands(&self, enabled: bool) -> Result<String, HttpCommandError> {
        let raw = if enabled { "启用" } else { "禁用" }.to_string();
        self.enqueue_remote_command(Self::remote_control_command(
            raw.clone(),
            &raw,
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled,
                username: "控制台".to_string(),
            }),
        ))
    }

    fn set_idle_exit(&self, minutes: u32) -> Result<String, HttpCommandError> {
        self.enqueue_remote_command(Self::remote_control_command(
            format!("闲置退出 {minutes}"),
            "闲置退出",
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes }),
        ))
    }

    fn clear_idle_exit(&self) -> Result<String, HttpCommandError> {
        let receipt = Self::required_receipt(self.tasks.enqueue_clear_idle_exit())?;
        Ok(json!({
            "ok": true,
            "queued": true,
            "taskId": receipt.task_id,
            "position": receipt.position,
            "command": "取消闲置退出"
        })
        .to_string())
    }

    fn list_workflows(&self) -> Result<String, HttpCommandError> {
        let workflows = self
            .custom_workflow
            .list()
            .into_iter()
            .map(|workflow| {
                json!({
                    "name": workflow.name,
                    "commands": workflow.commands,
                    "allowArgs": workflow.allow_args,
                    "confirmBeforeRun": workflow.confirm_before_run,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&workflows).map_err(internal_command_error)
    }

    fn run_workflow(&self, name: String, args: String) -> Result<String, HttpCommandError> {
        let prepared = self
            .custom_workflow
            .prepare_remote(&name, &args)
            .map_err(|error| HttpCommandError::bad_request(error.to_string()))?;
        self.enqueue_remote_command(Self::remote_control_command(
            prepared.raw,
            &prepared.matched,
            ModuleCommand::CustomWorkflow(prepared.command),
        ))
    }
}

fn internal_command_error(error: impl std::fmt::Display) -> HttpCommandError {
    HttpCommandError::internal(error.to_string())
}

fn validate_playable_track(track: &PlayableTrack) -> Result<(), HttpCommandError> {
    const MAX_ID_BYTES: usize = 512;
    const MAX_TEXT_BYTES: usize = 1024;
    const MAX_ARTISTS: usize = 32;
    const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;

    let id = track.track_ref.key.id.as_str();
    if id.trim().is_empty()
        || id != id.trim()
        || id.contains('/')
        || id.len() > MAX_ID_BYTES
        || id.chars().any(char::is_control)
    {
        return Err(HttpCommandError::bad_request("trackRef.key.id字段无效"));
    }
    validate_track_text(
        &track.metadata.title,
        "metadata.title",
        true,
        MAX_TEXT_BYTES,
    )?;
    if track.metadata.artists.len() > MAX_ARTISTS {
        return Err(HttpCommandError::bad_request("metadata.artists数量过多"));
    }
    for artist in &track.metadata.artists {
        validate_track_text(artist, "metadata.artists", true, MAX_TEXT_BYTES)?;
    }
    if let Some(album) = track.metadata.album.as_deref() {
        validate_track_text(album, "metadata.album", false, MAX_TEXT_BYTES)?;
    }
    if track
        .metadata
        .duration_ms
        .is_some_and(|duration| duration == 0 || duration > MAX_DURATION_MS)
    {
        return Err(HttpCommandError::bad_request("metadata.durationMs字段无效"));
    }
    validate_track_identity(
        track.track_ref.key.provider,
        id,
        track
            .track_ref
            .resolver_locator
            .as_ref()
            .map(|locator| locator.as_str()),
    )
}

fn validate_track_text(
    value: &str,
    field: &str,
    required: bool,
    max_bytes: usize,
) -> Result<(), HttpCommandError> {
    if (required && value.trim().is_empty())
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(HttpCommandError::bad_request(format!("{field}字段无效")));
    }
    Ok(())
}

fn validate_track_identity(
    provider: ProviderId,
    track_id: &str,
    locator: Option<&str>,
) -> Result<(), HttpCommandError> {
    match provider {
        ProviderId::QqMusic => {
            if !is_ascii_alphanumeric(track_id) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的QQ音乐MID",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(value) = locator.strip_prefix("qqmusic:v2:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some((locator_track, media_mid)) = value.split_once(':') else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !is_ascii_alphanumeric(locator_track) || !is_ascii_alphanumeric(media_mid) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Netease => {
            if !track_id.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的网易云歌曲ID",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(locator_track) = locator.strip_prefix("netease:v1:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !locator_track.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Bilibili => {
            if !is_bilibili_bvid(track_id) {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的B站BV号",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let Some(locator_track) = locator.strip_prefix("bilibili:v1:") else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if !is_bilibili_bvid(locator_track) {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
        ProviderId::Kugou => {
            if track_id.is_empty()
                || !track_id
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(HttpCommandError::bad_request(
                    "trackRef.key.id不是有效的酷狗歌曲Hash",
                ));
            }
            let Some(locator) = locator else {
                return Ok(());
            };
            let mut parts = locator.split(':');
            if parts.next() != Some("kugou") {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            let Some(locator_track) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some(album_id) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            let Some(album_audio_id) = parts.next() else {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            };
            if parts.next().is_some()
                || !locator_track
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
                || !album_id.bytes().all(|byte| byte.is_ascii_digit())
                || !album_audio_id.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(HttpCommandError::bad_request("resolverLocator格式无效"));
            }
            validate_locator_track(track_id, locator_track)
        }
    }
}

fn validate_locator_track(track_id: &str, locator_track: &str) -> Result<(), HttpCommandError> {
    if !locator_track.eq_ignore_ascii_case(track_id) {
        return Err(HttpCommandError::bad_request(
            "resolverLocator曲目与trackRef不一致",
        ));
    }
    Ok(())
}

fn is_ascii_alphanumeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_bilibili_bvid(value: &str) -> bool {
    value.starts_with("BV") && (8..=32).contains(&value.len()) && is_ascii_alphanumeric(value)
}

#[derive(Clone)]
pub(super) struct ApplicationHttpPlayerFacade {
    backend: PlayerRuntimeBackend,
    search: PlayerSearchClient,
    playback: PlaybackHandle,
    /// 与 PlaybackApplication 共享的播放模式句柄(0=顺序 1=单曲循环 2=随机)。
    play_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl ApplicationHttpPlayerFacade {
    pub(super) fn new(
        backend: PlayerRuntimeBackend,
        search: PlayerSearchClient,
        playback: PlaybackHandle,
        play_mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> Self {
        Self {
            backend,
            search,
            playback,
            play_mode,
        }
    }
}

impl HttpPlayerPort for ApplicationHttpPlayerFacade {
    fn status(&self) -> Result<PlayerStatus> {
        self.backend.status()
    }

    fn cache_stats(
        &self,
        keys: &[TrackKey],
    ) -> Result<(Option<AudioCacheStats>, Vec<AudioCacheTrackStatus>)> {
        Ok(self.playback.cache_stats(keys)?)
    }

    fn cached_tracks(&self, offset: usize, limit: usize) -> Result<CachedTrackPage> {
        Ok(self.playback.cached_tracks(offset, limit)?)
    }

    fn reset_track_statistics(&self, key: &TrackKey) -> Result<bool> {
        Ok(self.playback.reset_track_statistics(key)?)
    }

    fn invalidate_track_cache(&self, key: &TrackKey) -> Result<bool> {
        self.playback
            .invalidate_audio_cache(key)
            .map(|()| true)
            .map_err(anyhow::Error::from)
    }

    fn seek(&self, position_seconds: f64) -> Result<()> {
        self.playback
            .seek(position_seconds)
            .map_err(anyhow::Error::from)
    }

    fn play_mode(&self) -> Result<u8> {
        Ok(self.play_mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn set_play_mode(&self, mode: u8) -> Result<()> {
        if mode > 2 {
            anyhow::bail!("播放模式只允许 0(顺序)/1(单曲循环)/2(随机)");
        }
        self.play_mode
            .store(mode, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn search_text(&self, keyword: &str, source: &str) -> Result<String, HttpPlayerSearchError> {
        self.search
            .search_text(keyword, source)
            .map_err(player_search_error)
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, HttpPlayerSearchError> {
        self.search
            .search_candidates(keyword, source)
            .map_err(player_search_error)
    }
}

fn player_search_error(error: PlayerSearchClientError) -> HttpPlayerSearchError {
    let message = match error {
        PlayerSearchClientError::Failed(source) => source.to_string(),
        error => error.to_string(),
    };
    HttpPlayerSearchError::new(message)
}

#[derive(Clone)]
pub(super) struct ApplicationHttpLoginFacade {
    manager: LoginHelperManager,
}

impl ApplicationHttpLoginFacade {
    pub(super) fn new(manager: LoginHelperManager) -> Self {
        Self { manager }
    }
}

impl HttpLoginPort for ApplicationHttpLoginFacade {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError> {
        self.manager
            .providers()
            .map(|providers| providers.into_iter().map(provider_view).collect())
            .map_err(login_error)
    }

    fn status(&self) -> HttpLoginStatus {
        login_status(self.manager.status())
    }

    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError> {
        self.manager.start(provider).map_err(login_error)
    }

    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError> {
        self.manager.cancel(session_id).map_err(login_error)
    }

    fn logout(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError> {
        self.manager.logout(provider).map_err(login_error)
    }

    fn refresh(&self, provider: ProviderId) -> Result<CredentialStatus, HttpLoginError> {
        self.manager
            .refresh_credential(provider)
            .map_err(login_error)
    }

    fn kugou_status(&self) -> Result<miliastra_playback::KugouAccountStatus, HttpLoginError> {
        self.manager.kugou_status().map_err(login_error)
    }

    fn account_status(
        &self,
        provider: ProviderId,
    ) -> Result<Option<miliastra_playback::ProviderAccountStatus>, HttpLoginError> {
        self.manager.account_status(provider).map_err(login_error)
    }

    fn kugou_claim_vip(&self) -> Result<miliastra_playback::KugouListenReport, HttpLoginError> {
        self.manager.kugou_claim_vip().map_err(login_error)
    }

    fn kugou_upgrade_vip(&self) -> Result<miliastra_playback::KugouListenReport, HttpLoginError> {
        self.manager.kugou_upgrade_vip().map_err(login_error)
    }
}

fn provider_view(view: ProviderView) -> HttpProviderView {
    HttpProviderView {
        provider: view.provider,
        configured: view.configured,
        fields: view.fields,
        refresh_supported: view.refresh_supported,
        manual_refresh_supported: view.manual_refresh_supported,
        refresh_ready: view.refresh_ready,
        refresh_state: view.refresh_state,
        last_refresh_at_ms: view.last_refresh_at_ms,
        next_refresh_check_at_ms: view.next_refresh_check_at_ms,
        last_refresh_error: view.last_refresh_error,
    }
}

fn login_status(status: LoginManagerStatus) -> HttpLoginStatus {
    HttpLoginStatus {
        active: status.active,
        session_id: status.session_id,
        provider: status.provider,
        last_error: status.last_error.map(|error| HttpLoginErrorView {
            code: error.code,
            message: error.message,
            provider: error.provider,
        }),
    }
}

fn login_error(error: LoginHelperFailure) -> HttpLoginError {
    HttpLoginError {
        code: error.code,
        message: error.message,
    }
}

#[derive(Clone)]
pub(super) struct ApplicationHttpAiFacade {
    client: AiClient,
}

impl ApplicationHttpAiFacade {
    pub(super) fn new(client: AiClient) -> Self {
        Self { client }
    }
}

impl HttpAiPort for ApplicationHttpAiFacade {
    fn recognize(&self, query: &[(String, String)]) -> Result<String> {
        self.client.recognize_with_query(query)
    }

    fn match_song(&self, query: &[(String, String)]) -> Result<String> {
        self.client.match_with_query(query)
    }

    fn pick(&self, query: &[(String, String)]) -> Result<String> {
        self.client.pick_with_query(query)
    }
}
