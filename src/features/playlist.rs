//! 基于昵称归属的个人歌单收藏。
//!
//! 归属 owner = OCR 读到的昵称经 \`normalize_comparison_text\` 归一化后的结果，由调用方传入。
//! 存储与 tarot 一样复用统一数据库 \`playback.sqlite3\`，只新增自己的表，不改全局 schema 版本。
//! 采集阶段不在歌单内部去重；数据库层用 \`(owner, track_key)\` 主键去重，重复导入/收藏不会产生重复行。
use super::command::{CommandEnvelope, CommandPrefix, FeatureCommandMatch};
use super::reply_template::{ReplyTemplate, ReplyVars, playlist as templates};
use crate::features::chat_text::compact_command;
use crate::features::playback::PersonalPlaylistStart;
use crate::text::char_width;
use anyhow::{Result, anyhow};
use miliastra_playback::{PlayableTrack, PlaylistImportOperation};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 歌单窗口一次推荐的曲目数（\`@歌单\` 用 \`@1\`~\`@5\` 选择）。
pub(crate) const WINDOW_CANDIDATES: usize = 5;
/// 清空二次确认的有效期；超时后必须重新发起 \`@歌单清空\`。
const CLEAR_CONFIRM_TTL: Duration = Duration::from_secs(60);
/// 歌单窗口存活时间：超时后 @1~@5 不再生效。
pub(crate) const WINDOW_TTL: Duration = Duration::from_secs(20);
const IMPORT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_IMPORT_TRACKS: usize = 3000;

/// 一条待发送的歌单回复。
///
/// 为什么不是裸 String：歌单在业务层拼好整段文本，如果直接返回 String，
/// 组装层拿不到模板 key，用户在命令面板改的文案永远读不到。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlaylistReply {
    /// 已定型的文本（运行时数据、外部导入的歌单名等）。
    Raw(String),
    /// 带模板 key 的文案，发送时才查覆盖。
    Keyed {
        spec: &'static ReplyTemplate,
        vars: ReplyVars,
    },
}

impl PlaylistReply {
    fn keyed(spec: &'static ReplyTemplate, vars: ReplyVars) -> Self {
        Self::Keyed { spec, vars }
    }

    /// 按内置默认文案渲染，供测试与"只需成品文本"的场景使用。
    pub(crate) fn render_default(&self) -> String {
        match self {
            Self::Raw(text) => text.clone(),
            Self::Keyed { spec, vars } => {
                crate::features::reply_template::render(spec, None, vars).text
            }
        }
    }
}

impl From<String> for PlaylistReply {
    fn from(text: String) -> Self {
        Self::Raw(text)
    }
}

impl From<&str> for PlaylistReply {
    fn from(text: &str) -> Self {
        Self::Raw(text.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PlaylistCommand {
    Help,
    /// 打开歌单窗口：随机推荐若干首，用 @1~@5 选择。
    Open,
    /// 窗口内选择第 n 首（1~5）。
    Select(u8),
    /// 换一批：重抽，尽量避开本窗口已展示过的曲目。
    Reroll,
    /// 主动关闭窗口。
    Close,
    /// 收藏当前正在播放的曲目（无参数）。
    Favorite,
    /// 取消收藏当前正在播放的曲目。
    Unfavorite,
    /// 从自己的收藏随机推荐一首，不立即播放。
    Random,
    /// 启动个人收藏的随机连续播放，队列由收藏自动填充。
    PlayPlaylist,
    /// 回到大厅点歌模式。
    HallMode,
    /// 退出个人歌单播放模式（兼容旧别名）。
    StopPlaylist,
    /// 请求清空，进入二次确认。
    Clear,
    /// 确认清空。
    ConfirmClear,
    /// 放弃清空。
    AbortClear,
    Import {
        provider: String,
        input: String,
    },
    ConfirmImport,
    CancelImport,
}

impl PlaylistCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        Self::parse_chat(envelope).is_some()
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if envelope.prefix() != CommandPrefix::At {
            return None;
        }
        // OCR 会吞掉空格，因此用去空白后的压缩文本精确匹配。
        let text = compact_command(envelope.command_text());
        if let Some(input) = text
            .strip_prefix("导入QQ歌单")
            .filter(|value| !value.is_empty())
        {
            let input = input.to_owned();
            return Some(FeatureCommandMatch::new(
                text,
                envelope.command_text(),
                Self::Import {
                    provider: "qqmusic".to_owned(),
                    input,
                },
            ));
        }
        // `导入网易云歌单` 是 `导入网易歌单` 的隐藏别名，不进帮助。
        if let Some(input) = text
            .strip_prefix("导入网易歌单")
            .or_else(|| text.strip_prefix("导入网易云歌单"))
            .filter(|value| !value.is_empty())
        {
            let input = input.to_owned();
            return Some(FeatureCommandMatch::new(
                text,
                envelope.command_text(),
                Self::Import {
                    provider: "netease".to_owned(),
                    input,
                },
            ));
        }
        let command = match text.as_str() {
            "歌单" => Self::Open,
            "换一批" => Self::Reroll,
            "关闭" => Self::Close,
            "歌单帮助" => Self::Help,
            "收藏" => Self::Favorite,
            "取消收藏" | "歌单删除" => Self::Unfavorite,
            "歌单随机" | "随机歌单" => Self::Random,
            "播放歌单" | "歌单模式" => Self::PlayPlaylist,
            "大厅模式" | "退出歌单模式" => Self::HallMode,
            "停止播放歌单" => Self::StopPlaylist,
            "歌单清空" => Self::Clear,
            "确认清空" => Self::ConfirmClear,
            "放弃清空" => Self::AbortClear,
            "确认导入" => Self::ConfirmImport,
            "取消导入" => Self::CancelImport,
            _ => return None,
        };
        Some(FeatureCommandMatch::new(
            text,
            envelope.command_text(),
            command,
        ))
    }

    pub(crate) fn parse_window_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        Self::parse_chat(envelope).or_else(|| Self::parse_window_selection(envelope))
    }

    pub(crate) fn is_window_decision(envelope: &CommandEnvelope) -> bool {
        Self::parse_window_selection(envelope).is_some()
            || matches!(
                Self::parse_chat(envelope).map(|matched| matched.command),
                Some(Self::Reroll | Self::Close)
            )
    }

    fn parse_window_selection(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if envelope.prefix() != CommandPrefix::At {
            return None;
        }
        let text = compact_command(envelope.command_text());
        let index = text.parse::<u8>().ok()?;
        (1..=WINDOW_CANDIDATES as u8)
            .contains(&index)
            .then(|| FeatureCommandMatch::new(text, envelope.command_text(), Self::Select(index)))
    }

    /// 去重键按命令区分：不同歌单命令不再共用同一 lock_key 而被互相判重吞掉。
    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Help => "playlist:help",
            Self::Open => "playlist:open",
            Self::Select(index) => return format!("playlist:select:{index}"),
            Self::Reroll => "playlist:reroll",
            Self::Close => "playlist:close",
            Self::Favorite => "playlist:favorite",
            Self::Unfavorite => "playlist:unfavorite",
            Self::Random => "playlist:random",
            Self::PlayPlaylist => "playlist:play",
            Self::HallMode => "playlist:hall",
            Self::StopPlaylist => "playlist:stop",
            Self::Clear => "playlist:clear",
            Self::ConfirmClear => "playlist:confirm_clear",
            Self::AbortClear => "playlist:abort_clear",
            Self::Import { provider, .. } => return format!("playlist:import:{provider}"),
            Self::ConfirmImport => "playlist:confirm_import",
            Self::CancelImport => "playlist:cancel_import",
        }
        .to_string()
    }
}

/// 收藏能力的播放侧依赖。抽成端口以便单测不依赖真实播放器。
pub(crate) trait PlaylistPlaybackPort {
    /// 当前正在播放的曲目；未播放时为 None。
    fn current_track(&self) -> Option<PlayableTrack>;
    /// 播放一首曲目。
    #[allow(dead_code)]
    fn play(&mut self, track: &PlayableTrack) -> Result<()>;
    fn import_playlist(&mut self, provider: &str, input: &str) -> Result<PlaylistImportOperation>;
    fn enqueue_next(&mut self, _track: &PlayableTrack) -> Result<bool> {
        anyhow::bail!("播放队列不可用")
    }
    fn start_playlist(
        &mut self,
        _owner: &str,
        _tracks: Vec<PlayableTrack>,
    ) -> Result<PersonalPlaylistStart> {
        anyhow::bail!("个人歌单播放模式不可用")
    }
    fn stop_playlist(&mut self) -> Result<bool> {
        anyhow::bail!("个人歌单播放模式不可用")
    }
    /// 当前歌单模式的归属昵称；未激活为 None。
    fn playlist_mode_owner(&self) -> Option<String> {
        None
    }
    /// owner 与当前模式匹配时，把新曲目并入运行中的随机池（导入用）。
    fn extend_playlist_tracks(&mut self, _owner: &str, _tracks: &[PlayableTrack]) -> Result<bool> {
        Ok(false)
    }
}

/// 歌单窗口：展示候选、记录已展示集合与截止时间。
struct PlaylistWindow {
    tracks: Vec<PlayableTrack>,
    shown: Vec<String>,
    deadline: Instant,
}

enum PendingPlaylistImport {
    Loading {
        operation: PlaylistImportOperation,
        deadline: Instant,
    },
    Ready {
        playlist: miliastra_playback::ImportedPlaylist,
        deadline: Instant,
    },
}

struct PlaylistState {
    connection: Mutex<Connection>,
    /// 每个 owner 的清空确认截止时间。
    pending_clear: Mutex<HashMap<String, Instant>>,
    /// 每个 owner 的当前歌单窗口。
    windows: Mutex<HashMap<String, PlaylistWindow>>,
    pending_imports: Mutex<HashMap<String, PendingPlaylistImport>>,
}

#[derive(Clone)]
pub(crate) struct PlaylistApplication {
    state: Arc<PlaylistState>,
}

/// 歌单归属键：**只用身份 UUID**。
///
/// 早期版本按昵称归属（`name:`），但昵称会改、会重名、OCR 会读错。
/// 现在歌单只属于已在「身份映射」登记过的玩家——没有 UUID 就没有歌单，
/// 这与"歌单仅对好友及以上开放"的权限设计一致。
pub(crate) mod owner_key {
    pub(crate) const IDENTITY: &str = "id:";

    /// 按身份 UUID 构造键。
    pub(crate) fn for_identity(uuid: &str) -> String {
        format!("{IDENTITY}{}", uuid.trim())
    }

    /// 读取候选键：没有身份映射时返回空——调用方据此判定"无歌单权限"。
    pub(crate) fn candidates(identity: Option<&str>) -> Vec<String> {
        identity
            .filter(|value| !value.trim().is_empty())
            .map(|uuid| vec![for_identity(uuid)])
            .unwrap_or_default()
    }

    /// 迁移旧键：把 `name:<昵称>` 记录改写成 `id:<uuid>`。
    ///
    /// 只迁移能在身份映射里找到同名昵称的行；找不到的保留原样（读不到，
    /// 但数据不丢），等用户补配映射后下次启动再迁。
    pub(crate) fn migrate_named_rows(
        connection: &rusqlite::Connection,
        nickname_to_uuid: &std::collections::HashMap<String, String>,
    ) -> rusqlite::Result<usize> {
        if nickname_to_uuid.is_empty() {
            return Ok(0);
        }
        let mut migrated = 0usize;
        for (nickname, uuid) in nickname_to_uuid {
            migrated += connection.execute(
                "UPDATE OR REPLACE playlist_items
                 SET owner = ?1
                 WHERE owner = ?2",
                rusqlite::params![
                    for_identity(uuid),
                    format!("name:{}", crate::text::normalize_comparison_text(nickname.trim())),
                ],
            )?;
            // 兼容极早期未加前缀的裸昵称。
            migrated += connection.execute(
                "UPDATE OR REPLACE playlist_items
                 SET owner = ?1
                 WHERE owner = ?2",
                rusqlite::params![for_identity(uuid), nickname.trim()],
            )?;
        }
        Ok(migrated)
    }
}

impl PlaylistApplication {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub(crate) fn from_connection(connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(3))?;
        // 独立建表，保留其他功能既有数据，不改 PRAGMA user_version。
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS playlist_items (
                 owner    TEXT    NOT NULL,
                 track_key TEXT   NOT NULL,
                 track_json TEXT  NOT NULL,
                 added_at INTEGER NOT NULL,
                 PRIMARY KEY (owner, track_key)
             );
             CREATE INDEX IF NOT EXISTS idx_playlist_items_owner ON playlist_items(owner);",
        )?;
        // 旧键迁移到 UUID 由 `migrate_to_identity_keys` 在配置加载后执行：
        // 这里拿不到身份映射，不能在此处迁移。
        Ok(Self {
            state: Arc::new(PlaylistState {
                connection: Mutex::new(connection),
                pending_clear: Mutex::new(HashMap::new()),
                windows: Mutex::new(HashMap::new()),
                pending_imports: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// 把旧的昵称键迁移到身份 UUID 键。
    ///
    /// 必须在身份映射加载后调用（`PlaylistApplication::open` 时还不知道映射）。
    /// 幂等：没有可迁移的行时什么都不做；找不到映射的行原样保留。
    pub(crate) fn migrate_to_identity_keys(
        &self,
        nickname_to_uuid: &std::collections::HashMap<String, String>,
    ) -> Result<usize> {
        let connection = self.connection()?;
        let migrated = owner_key::migrate_named_rows(&connection, nickname_to_uuid)?;
        if migrated > 0 {
            log::info!("歌单归属键迁移：{migrated} 条记录已改为身份 UUID");
        }
        Ok(migrated)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.state
            .connection
            .lock()
            .map_err(|_| anyhow!("歌单存储锁已中毒"))
    }

    fn add_many(
        &self,
        owner: &str,
        tracks: &[PlayableTrack],
        now_epoch: i64,
    ) -> Result<(usize, usize)> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut added = 0;
        let mut duplicates = 0;
        for track in tracks {
            let track_key = track.track_ref.key.to_string();
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM playlist_items WHERE owner = ?1 AND track_key = ?2)",
                params![owner, track_key],
                |row| row.get(0),
            )?;
            if exists {
                duplicates += 1;
            } else {
                added += 1;
            }
            let track_json = serde_json::to_string(track)?;
            transaction.execute(
                "INSERT INTO playlist_items(owner, track_key, track_json, added_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(owner, track_key) DO UPDATE SET track_json = excluded.track_json",
                params![owner, track_key, track_json, now_epoch],
            )?;
        }
        transaction.commit()?;
        Ok((added, duplicates))
    }

    /// 收藏一首曲目。返回 true 表示新增，false 表示已存在（数据库层去重）。
    pub(crate) fn add(&self, owner: &str, track: &PlayableTrack, now_epoch: i64) -> Result<bool> {
        let track_key = track.track_ref.key.to_string();
        let track_json = serde_json::to_string(track)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlist_items WHERE owner = ?1 AND track_key = ?2)",
            params![owner, track_key],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO playlist_items(owner, track_key, track_json, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner, track_key) DO UPDATE SET track_json = excluded.track_json",
            params![owner, track_key, track_json, now_epoch],
        )?;
        transaction.commit()?;
        Ok(!exists)
    }

    /// 从收藏移除一首曲目。返回 true 表示确实移除了。
    pub(crate) fn remove(&self, owner: &str, track: &PlayableTrack) -> Result<bool> {
        let track_key = track.track_ref.key.to_string();
        let connection = self.connection()?;
        let changed = connection.execute(
            "DELETE FROM playlist_items WHERE owner = ?1 AND track_key = ?2",
            params![owner, track_key],
        )?;
        Ok(changed > 0)
    }

    /// 清空一个 owner 的全部收藏，返回删除条数。
    pub(crate) fn clear(&self, owner: &str) -> Result<usize> {
        let connection = self.connection()?;
        let changed = connection.execute(
            "DELETE FROM playlist_items WHERE owner = ?1",
            params![owner],
        )?;
        Ok(changed)
    }

    fn count_existing_tracks(&self, owner: &str, tracks: &[PlayableTrack]) -> Result<usize> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT EXISTS(SELECT 1 FROM playlist_items WHERE owner = ?1 AND track_key = ?2)",
        )?;
        let mut count = 0;
        let mut seen = std::collections::HashSet::new();
        for track in tracks {
            let key = track.track_ref.key.to_string();
            let already_present = !seen.insert(key.clone())
                || statement.query_row(params![owner, key], |row| row.get::<_, bool>(0))?;
            count += usize::from(already_present);
        }
        Ok(count)
    }

    pub(crate) fn count(&self, owner: &str) -> Result<usize> {
        let connection = self.connection()?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM playlist_items WHERE owner = ?1",
            params![owner],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// 按入库顺序读取全部收藏。
    /// 合并所有候选键下的曲目。
    ///
    /// 不能"第一个非空就返回"：迁移后老数据在 `name:` 键，新增的收藏会写到
    /// `id:` 键，只取第一个会让用户看到"歌单变空了"。合并 + 去重才是对的。
    pub(crate) fn items_for(&self, candidates: &[String]) -> Result<Vec<PlayableTrack>> {
        let mut merged: Vec<PlayableTrack> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for key in candidates {
            for item in self.items(key)? {
                // 同一首歌可能同时存在于迁移前的 name: 键与迁移后的 id: 键下。
                if seen.insert(item.track_ref.key.to_string()) {
                    merged.push(item);
                }
            }
        }
        Ok(merged)
    }

    /// 按候选键查总数：与 [`Self::items_for`] 用同一个"第一个命中"规则。
    pub(crate) fn count_for(&self, candidates: &[String]) -> Result<usize> {
        for key in candidates {
            let count = self.count(key)?;
            if count > 0 {
                return Ok(count);
            }
        }
        Ok(0)
    }

    pub(crate) fn items(&self, owner: &str) -> Result<Vec<PlayableTrack>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT track_json FROM playlist_items WHERE owner = ?1 ORDER BY added_at ASC, track_key ASC",
        )?;
        let rows = statement.query_map(params![owner], |row| row.get::<_, String>(0))?;
        let mut tracks = Vec::new();
        for row in rows {
            let json = row?;
            match serde_json::from_str::<PlayableTrack>(&json) {
                Ok(track) => tracks.push(track),
                // 单条损坏不应让整个歌单不可用。
                Err(error) => log::warn!("歌单条目解析失败，已跳过: {error}"),
            }
        }
        Ok(tracks)
    }

    /// 随机挑一首，尽量避开 exclude 中的 track_key。
    pub(crate) fn pick_random(
        &self,
        owner: &str,
        exclude: &[String],
        seed: u64,
    ) -> Result<Option<PlayableTrack>> {
        let tracks = self.items(owner)?;
        if tracks.is_empty() {
            return Ok(None);
        }
        let filtered: Vec<PlayableTrack> = tracks
            .iter()
            .filter(|track| !exclude.contains(&track.track_ref.key.to_string()))
            .cloned()
            .collect();
        let pool = if filtered.is_empty() {
            tracks
        } else {
            filtered
        };
        let index = (seed % pool.len() as u64) as usize;
        Ok(pool.into_iter().nth(index))
    }

    /// 随机挑最多 count 首；本周期有剩余时不补旧歌，耗尽后重新开始。
    fn pick_many(
        &self,
        owner: &str,
        exclude: &[String],
        count: usize,
        seed: u64,
    ) -> Result<Vec<PlayableTrack>> {
        let all = self.items(owner)?;
        if all.is_empty() {
            return Ok(Vec::new());
        }
        let mut pool: Vec<PlayableTrack> = all
            .iter()
            .filter(|track| !exclude.contains(&track.track_ref.key.to_string()))
            .cloned()
            .collect();
        if pool.is_empty() {
            pool = all;
        }
        let mut order: Vec<usize> = (0..pool.len()).collect();
        // 确定性洗牌：同一 seed 结果稳定，便于测试。
        let mut state = seed | 1;
        for index in (1..order.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let swap_with = ((state >> 33) as usize) % (index + 1);
            order.swap(index, swap_with);
        }
        Ok(order
            .into_iter()
            .take(count.min(pool.len()))
            .filter_map(|index| pool.get(index).cloned())
            .collect())
    }

    /// 打开或刷新窗口，返回本次展示的曲目。
    fn store_window(
        &self,
        owner: &str,
        tracks: Vec<PlayableTrack>,
        shown: Vec<String>,
        now: Instant,
        reset_deadline: bool,
    ) -> Result<()> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        windows.retain(|_, window| window.deadline > now);
        let deadline = if reset_deadline {
            now + WINDOW_TTL
        } else {
            windows
                .get(owner)
                .map_or(now + WINDOW_TTL, |window| window.deadline)
        };
        windows.insert(
            owner.to_owned(),
            PlaylistWindow {
                tracks,
                shown,
                // 重抽时从新一批候选重置期限。
                deadline,
            },
        );
        Ok(())
    }

    fn window_shown(&self, owner: &str, now: Instant) -> Result<Option<Vec<String>>> {
        let windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        Ok(windows
            .get(owner)
            .filter(|window| window.deadline > now)
            .map(|window| window.shown.clone()))
    }

    /// 取出窗口并按下标选曲；窗口过期或不存在时返回 None。
    fn take_window_selection(
        &self,
        owner: &str,
        index: usize,
        now: Instant,
    ) -> Result<Option<PlayableTrack>> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        let Some(window) = windows.get(owner) else {
            return Ok(None);
        };
        if window.deadline <= now {
            windows.remove(owner);
            return Ok(None);
        }
        let track = window.tracks.get(index).cloned();
        if track.is_some() {
            windows.remove(owner);
        }
        Ok(track)
    }

    pub(crate) fn window_len(&self, owner: &str, now: Instant) -> Result<usize> {
        let windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        Ok(windows
            .get(owner)
            .filter(|window| window.deadline > now)
            .map_or(0, |window| window.tracks.len()))
    }

    pub(crate) fn has_command_context(
        &self,
        owner: &str,
        command: &PlaylistCommand,
        now: Instant,
    ) -> Result<bool> {
        match command {
            PlaylistCommand::Select(_) | PlaylistCommand::Reroll | PlaylistCommand::Close => {
                Ok(self.window_len(owner, now)? > 0)
            }
            PlaylistCommand::ConfirmImport | PlaylistCommand::CancelImport => {
                let imports = self
                    .state
                    .pending_imports
                    .lock()
                    .map_err(|_| anyhow!("歌单导入状态不可用"))?;
                Ok(matches!(
                    imports.get(owner),
                    Some(
                        PendingPlaylistImport::Loading { deadline, .. }
                            | PendingPlaylistImport::Ready { deadline, .. }
                    ) if *deadline > now
                ))
            }
            PlaylistCommand::ConfirmClear | PlaylistCommand::AbortClear => {
                let pending = self
                    .state
                    .pending_clear
                    .lock()
                    .map_err(|_| anyhow!("歌单确认状态不可用"))?;
                Ok(pending.get(owner).is_some_and(|deadline| *deadline > now))
            }
            _ => Ok(true),
        }
    }

    pub(crate) fn discard_window(&self, owner: &str) -> Result<()> {
        self.state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?
            .remove(owner);
        Ok(())
    }

    pub(crate) fn close_active_window(&self, owner: &str, now: Instant) -> Result<bool> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        let active = windows
            .get(owner)
            .is_some_and(|window| window.deadline > now);
        windows.remove(owner);
        Ok(active)
    }

    /// 应用层独占窗口包含建立共享基线的时间，覆盖存储层默认 TTL。
    pub(crate) fn set_window_deadline(&self, owner: &str, deadline: Instant) -> Result<()> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        if let Some(window) = windows.get_mut(owner) {
            window.deadline = deadline;
        }
        Ok(())
    }

    fn mark_pending_clear(&self, owner: &str, now: Instant) -> Result<()> {
        let mut pending = self
            .state
            .pending_clear
            .lock()
            .map_err(|_| anyhow!("歌单确认状态不可用"))?;
        pending.retain(|_, deadline| *deadline > now);
        pending.insert(owner.to_owned(), now + CLEAR_CONFIRM_TTL);
        Ok(())
    }

    fn take_pending_clear(&self, owner: &str, now: Instant) -> Result<bool> {
        let mut pending = self
            .state
            .pending_clear
            .lock()
            .map_err(|_| anyhow!("歌单确认状态不可用"))?;
        match pending.remove(owner) {
            Some(deadline) => Ok(deadline > now),
            None => Ok(false),
        }
    }

    pub(crate) fn poll_import_completions(&self, now: Instant) -> Result<Vec<Vec<PlaylistReply>>> {
        let mut imports = self
            .state
            .pending_imports
            .lock()
            .map_err(|_| anyhow!("歌单导入状态不可用"))?;
        let mut completed = Vec::new();
        for (owner, pending) in imports.iter_mut() {
            match pending {
                PendingPlaylistImport::Loading {
                    operation,
                    deadline,
                } if *deadline <= now => {
                    completed.push((owner.clone(), None));
                }
                PendingPlaylistImport::Loading { operation, .. } => {
                    if let Some(result) = operation.try_wait() {
                        completed.push((owner.clone(), Some(result)));
                    }
                }
                PendingPlaylistImport::Ready { deadline, .. } if *deadline <= now => {
                    completed.push((owner.clone(), None));
                }
                PendingPlaylistImport::Ready { .. } => {}
            }
        }
        let mut replies = Vec::new();
        for (owner, result) in completed {
            match result {
                None => {
                    imports.remove(&owner);
                    replies.push(vec![PlaylistReply::keyed(
                        &templates::IMPORT_TIMEOUT,
                        ReplyVars::new(),
                    )]);
                }
                Some(Err(_)) => {
                    imports.remove(&owner);
                    replies.push(vec![PlaylistReply::keyed(
                        &templates::IMPORT_FAILED,
                        ReplyVars::new(),
                    )]);
                }
                Some(Ok(playlist)) if playlist.tracks.len() > MAX_IMPORT_TRACKS => {
                    imports.remove(&owner);
                    replies.push(vec![PlaylistReply::keyed(
                        &templates::IMPORT_TOO_LARGE,
                        ReplyVars::new().with("上限", MAX_IMPORT_TRACKS.to_string()),
                    )]);
                }
                Some(Ok(playlist)) => {
                    let total = playlist.tracks.len();
                    let duplicate_count = self.count_existing_tracks(&owner, &playlist.tracks)?;
                    let new_count = total.saturating_sub(duplicate_count);
                    let creator = playlist.creator.as_deref().unwrap_or("未知");
                    let provider = if playlist.provider == "qqmusic" {
                        "QQ音乐"
                    } else {
                        "网易云音乐"
                    };
                    let mut preview: Vec<PlaylistReply> = vec![
                        "歌单导入预览".to_owned().into(),
                        format!("{provider}: {}", chat_safe_text(&playlist.title, 34)).into(),
                        format!("创建者: {}", chat_safe_text(creator, 32)).into(),
                        format!("曲目: {total}首,新增{new_count}首,已收藏{duplicate_count}首")
                            .into(),
                    ];
                    preview.extend(playlist.tracks.iter().take(3).enumerate().map(
                        |(index, track)| {
                            PlaylistReply::from(format!(
                                "示例{}: {} - {}",
                                index + 1,
                                chat_safe_text(&track.metadata.title, 22),
                                chat_safe_text(&track_artists(track), 18)
                            ))
                        },
                    ));
                    preview.push("回复@确认导入或@取消导入.".to_owned().into());
                    imports.insert(
                        owner,
                        PendingPlaylistImport::Ready {
                            playlist,
                            deadline: now + IMPORT_TTL,
                        },
                    );
                    replies.push(preview);
                }
            }
        }
        Ok(replies)
    }

    pub(crate) fn execute(
        &self,
        owner: &str,
        command: &PlaylistCommand,
        playback: &mut dyn PlaylistPlaybackPort,
        now: Instant,
        now_epoch: i64,
    ) -> Result<Vec<PlaylistReply>> {
        let owner = owner.trim();
        if owner.is_empty() {
            return Ok(vec![PlaylistReply::keyed(
                &templates::ACTOR_MISSING,
                ReplyVars::new(),
            )]);
        }
        match command {
            // 帮助是单条模板、多行内容：拆条由发送路径负责。
            PlaylistCommand::Help => Ok(vec![PlaylistReply::keyed(
                &crate::features::reply_template::HELP_PLAYLIST,
                ReplyVars::new(),
            )]),
            PlaylistCommand::Open => {
                self.discard_window(owner)?;
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::EMPTY_NO_TRACK_HINT,
                        ReplyVars::new(),
                    )]);
                }
                let seed = seed_from(now_epoch, owner);
                let tracks = self.pick_many(owner, &[], WINDOW_CANDIDATES, seed)?;
                let shown: Vec<String> = tracks
                    .iter()
                    .map(|track| track.track_ref.key.to_string())
                    .collect();
                self.store_window(owner, tracks.clone(), shown, now, false)?;
                Ok(window_messages(&tracks))
            }
            PlaylistCommand::Select(index) => {
                let index = (*index as usize).saturating_sub(1);
                match self.take_window_selection(owner, index, now)? {
                    Some(track) => {
                        if !playback.enqueue_next(&track)? {
                            return Ok(vec![PlaylistReply::keyed(
                                &templates::SELECT_QUEUE_FULL,
                                ReplyVars::new(),
                            )]);
                        }
                        Ok(vec![PlaylistReply::keyed(
                            &templates::SELECT_QUEUED,
                            ReplyVars::new()
                                .with("歌名", track_title(&track))
                                .with("歌手", track_artists(&track)),
                        )])
                    }
                    None => {
                        let len = self.window_len(owner, now)?;
                        if len == 0 {
                            Ok(Vec::new())
                        } else {
                            Ok(vec![PlaylistReply::keyed(
                                &templates::SELECT_OUT_OF_RANGE,
                                ReplyVars::new().with("上限", len.to_string()),
                            )])
                        }
                    }
                }
            }
            PlaylistCommand::Reroll => {
                let Some(shown) = self.window_shown(owner, now)? else {
                    return Ok(Vec::new());
                };
                let seed = seed_from(now_epoch ^ 0x5DEE_CE66, owner);
                let tracks = self.pick_many(owner, &shown, WINDOW_CANDIDATES, seed)?;
                if tracks.is_empty() {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::REROLL_EXHAUSTED,
                        ReplyVars::new(),
                    )]);
                }
                // pick_many 只有未展示池耗尽才回到全部曲目，此时重置周期。
                let restarted = tracks
                    .iter()
                    .all(|track| shown.contains(&track.track_ref.key.to_string()));
                let mut all_shown = if restarted { Vec::new() } else { shown };
                all_shown.extend(tracks.iter().map(|track| track.track_ref.key.to_string()));
                self.store_window(owner, tracks.clone(), all_shown, now, true)?;
                Ok(window_messages(&tracks))
            }
            PlaylistCommand::Close => {
                if self.close_active_window(owner, now)? {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::WINDOW_CLOSED,
                        ReplyVars::new(),
                    )])
                } else {
                    Ok(Vec::new())
                }
            }
            PlaylistCommand::Favorite => {
                let Some(track) = playback.current_track() else {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::FAVORITE_NO_TRACK,
                        ReplyVars::new(),
                    )]);
                };
                let title = track_title(&track);
                let added = self.add(owner, &track, now_epoch)?;
                if added {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::FAVORITE_ADDED,
                        ReplyVars::new()
                            .with("歌名", title)
                            .with("数量", self.count(owner)?.to_string()),
                    )])
                } else {
                    // 歌单模式下当前曲通常已在收藏里；明确提示，不让人误以为又收藏了一首。
                    Ok(vec![PlaylistReply::keyed(
                        &templates::FAVORITE_EXISTS,
                        ReplyVars::new().with("歌名", title),
                    )])
                }
            }
            PlaylistCommand::Unfavorite => {
                let Some(track) = playback.current_track() else {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::UNFAVORITE_NO_TRACK,
                        ReplyVars::new(),
                    )]);
                };
                let title = track_title(&track);
                let removed = self.remove(owner, &track)?;
                if removed {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::UNFAVORITE_REMOVED,
                        ReplyVars::new()
                            .with("歌名", title)
                            .with("数量", self.count(owner)?.to_string()),
                    )])
                } else {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::UNFAVORITE_MISSING,
                        ReplyVars::new().with("歌名", title),
                    )])
                }
            }
            PlaylistCommand::PlayPlaylist => {
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::PLAY_EMPTY,
                        ReplyVars::new(),
                    )]);
                }
                let tracks = self.pick_many(owner, &[], total, seed_from(now_epoch, owner))?;
                match playback.start_playlist(owner, tracks)? {
                    PersonalPlaylistStart::Started(queued) => Ok(vec![PlaylistReply::keyed(
                        &templates::PLAY_STARTED,
                        ReplyVars::new().with("数量", queued.to_string()),
                    )]),
                    PersonalPlaylistStart::AlreadyActive { owner: current } => {
                        Ok(vec![PlaylistReply::keyed(
                            &templates::PLAY_OCCUPIED,
                            ReplyVars::new().with("歌手", current),
                        )])
                    }
                }
            }
            PlaylistCommand::HallMode | PlaylistCommand::StopPlaylist => {
                if playback.stop_playlist()? {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::MODE_EXITED,
                        ReplyVars::new(),
                    )])
                } else {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::MODE_ALREADY_HALL,
                        ReplyVars::new(),
                    )])
                }
            }
            PlaylistCommand::Random => {
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::EMPTY_NO_TRACK_HINT,
                        ReplyVars::new(),
                    )]);
                }
                let seed = seed_from(now_epoch, owner);
                let Some(track) = self.pick_random(owner, &[], seed)? else {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::EMPTY_PLAIN,
                        ReplyVars::new(),
                    )]);
                };
                Ok(vec![PlaylistReply::keyed(
                    &templates::RANDOM_PICKED,
                    ReplyVars::new()
                        .with("歌名", track_title(&track))
                        .with("数量", total.to_string()),
                )])
            }
            PlaylistCommand::Import { provider, input } => {
                let mut imports = self
                    .state
                    .pending_imports
                    .lock()
                    .map_err(|_| anyhow!("歌单导入状态不可用"))?;
                imports.retain(|_, pending| match pending {
                    PendingPlaylistImport::Loading { deadline, .. }
                    | PendingPlaylistImport::Ready { deadline, .. } => *deadline > now,
                });
                if imports.contains_key(owner) {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::IMPORT_PENDING,
                        ReplyVars::new(),
                    )]);
                }
                if imports.len() >= 4 {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::IMPORT_BUSY,
                        ReplyVars::new(),
                    )]);
                }
                let operation = playback.import_playlist(provider, input)?;
                imports.insert(
                    owner.to_owned(),
                    PendingPlaylistImport::Loading {
                        operation,
                        deadline: now + IMPORT_TTL,
                    },
                );
                Ok(vec![PlaylistReply::keyed(
                    &templates::IMPORT_LOADING,
                    ReplyVars::new(),
                )])
            }
            PlaylistCommand::ConfirmImport => {
                let mut imports = self
                    .state
                    .pending_imports
                    .lock()
                    .map_err(|_| anyhow!("歌单导入状态不可用"))?;
                let Some(pending) = imports.remove(owner) else {
                    return Ok(Vec::new());
                };
                match pending {
                    PendingPlaylistImport::Loading {
                        operation,
                        deadline,
                    } if deadline > now => {
                        imports.insert(
                            owner.to_owned(),
                            PendingPlaylistImport::Loading {
                                operation,
                                deadline,
                            },
                        );
                        Ok(vec![PlaylistReply::keyed(
                            &templates::IMPORT_STILL_LOADING,
                            ReplyVars::new(),
                        )])
                    }
                    PendingPlaylistImport::Ready { playlist, deadline } if deadline > now => {
                        drop(imports);
                        let (added, duplicates) =
                            self.add_many(owner, &playlist.tracks, now_epoch)?;
                        // 导入是往歌单里加歌：新歌并入运行中的随机池（owner 匹配时）。
                        playback.extend_playlist_tracks(owner, &playlist.tracks)?;
                        Ok(vec![PlaylistReply::keyed(
                            &templates::IMPORT_DONE,
                            ReplyVars::new()
                                .with("歌名", safe_external_text(&playlist.title))
                                .with("新增", added.to_string())
                                .with("重复", duplicates.to_string()),
                        )])
                    }
                    _ => Ok(Vec::new()),
                }
            }
            PlaylistCommand::CancelImport => {
                let removed = self
                    .state
                    .pending_imports
                    .lock()
                    .map_err(|_| anyhow!("歌单导入状态不可用"))?
                    .remove(owner)
                    .is_some_and(|pending| match pending {
                        PendingPlaylistImport::Loading { deadline, .. }
                        | PendingPlaylistImport::Ready { deadline, .. } => deadline > now,
                    });
                if removed {
                    Ok(vec![PlaylistReply::keyed(
                        &templates::IMPORT_CANCELLED,
                        ReplyVars::new(),
                    )])
                } else {
                    Ok(Vec::new())
                }
            }
            PlaylistCommand::Clear => {
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::CLEAR_ALREADY_EMPTY,
                        ReplyVars::new(),
                    )]);
                }
                self.mark_pending_clear(owner, now)?;
                Ok(vec![PlaylistReply::keyed(
                    &templates::CLEAR_CONFIRM,
                    ReplyVars::new().with("数量", total.to_string()),
                )])
            }
            PlaylistCommand::ConfirmClear => {
                if !self.take_pending_clear(owner, now)? {
                    return Ok(Vec::new());
                }
                let removed = self.clear(owner)?;
                if playback.playlist_mode_owner().as_deref() == Some(owner) {
                    // 运行中的模式没有可播内容，直接退出，避免“已清空还在放”。
                    playback.stop_playlist()?;
                    return Ok(vec![PlaylistReply::keyed(
                        &templates::CLEAR_DONE_WITH_EXIT,
                        ReplyVars::new().with("数量", removed.to_string()),
                    )]);
                }
                Ok(vec![PlaylistReply::keyed(
                    &templates::CLEAR_DONE,
                    ReplyVars::new().with("数量", removed.to_string()),
                )])
            }
            PlaylistCommand::AbortClear => {
                if !self.take_pending_clear(owner, now)? {
                    return Ok(Vec::new());
                }
                Ok(vec![PlaylistReply::keyed(
                    &templates::CLEAR_ABORTED,
                    ReplyVars::new(),
                )])
            }
        }
    }
}

/// 每首歌单独发一条；五首推荐时首尾说明加五首正好七条。
fn window_messages(tracks: &[PlayableTrack]) -> Vec<PlaylistReply> {
    let mut messages = vec![PlaylistReply::keyed(
        &templates::WINDOW_HEADER,
        ReplyVars::new().with("数量", tracks.len().to_string()),
    )];
    messages.extend(tracks.iter().enumerate().map(|(index, track)| {
        PlaylistReply::keyed(
            &templates::WINDOW_ITEM,
            ReplyVars::new()
                .with("序号", (index + 1).to_string())
                .with("歌名", chat_safe_text(&track.metadata.title, 24))
                .with("歌手", chat_safe_text(&track_artists(track), 20)),
        )
    }));
    if !tracks.is_empty() {
        messages.push(PlaylistReply::keyed(
            &templates::WINDOW_FOOTER,
            ReplyVars::new().with(
                "上限",
                tracks.len().min(WINDOW_CANDIDATES).to_string(),
            ),
        ));
    }
    messages
}

fn track_artists(track: &PlayableTrack) -> String {
    track.metadata.artists.join(" / ")
}

fn chat_safe_text(text: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        let next_width = char_width(ch);
        if width + next_width > max_width {
            output.push('…');
            break;
        }
        output.push(ch);
        width += next_width;
    }
    output.trim().to_owned()
}

fn safe_external_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(60)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn track_title(track: &PlayableTrack) -> String {
    let title = track.metadata.title.trim();
    if title.is_empty() {
        track.track_ref.key.to_string()
    } else {
        title.to_owned()
    }
}

fn seed_from(now_epoch: i64, owner: &str) -> u64 {
    let mut hash = 1469598103934665603u64;
    for byte in owner.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    hash ^ (now_epoch as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    mod owner_key_tests {
        use super::super::{owner_key, PlaylistApplication};

        #[test]
        fn owner_key_is_identity_only() {
            // 歌单只认身份 UUID：昵称/UUID 都不再是兜底键。
            assert_eq!(
                owner_key::candidates(Some("uuid-1")),
                vec!["id:uuid-1".to_string()]
            );
            // 没有身份映射 → 空候选，调用方据此拒绝操作。
            assert!(owner_key::candidates(None).is_empty());
            assert!(owner_key::candidates(Some("   ")).is_empty());
        }

        #[test]
        fn identity_anchor_is_required_for_playlist() {
            // 路人（没有身份映射）不能拥有歌单：候选为空，调用方据此拒绝操作。
            assert!(owner_key::candidates(None).is_empty());
            assert!(owner_key::candidates(Some("")).is_empty());
            // 有身份时只有一个键，且就是 UUID。
            assert_eq!(
                owner_key::candidates(Some("uuid-1")),
                vec!["id:uuid-1".to_string()]
            );
        }

        #[test]
        fn migration_rewrites_legacy_name_keys_to_uuid() {
            let dir = std::env::temp_dir().join(format!(
                "playlist-uuid-migrate-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("playlist.sqlite3");
            {
                let app = PlaylistApplication::open(&path).unwrap();
                let guard = app.connection().unwrap();
                // 三种历史形态：裸昵称、name: 前缀、已迁移的 id:。
                guard
                    .execute(
                        "INSERT INTO playlist_items(owner, track_key, track_json, added_at)
                         VALUES('洛','k1','{}',1),
                               ('name:小明','k2','{}',2),
                               ('id:uuid-existing','k3','{}',3)",
                        [],
                    )
                    .unwrap();
                drop(guard);
                drop(app);
            }
            let app = PlaylistApplication::open(&path).unwrap();
            let mappings = std::collections::HashMap::from([
                ("洛".to_string(), "uuid-luo".to_string()),
                ("小明".to_string(), "uuid-ming".to_string()),
            ]);
            app.migrate_to_identity_keys(&mappings).unwrap();
            let owners: Vec<String> = {
                let guard = app.connection().unwrap();
                let rows = guard
                    .prepare("SELECT owner FROM playlist_items ORDER BY owner")
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .map(|row| row.unwrap())
                    .collect();
                rows
            };
            drop(app);
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(
                owners,
                vec![
                    "id:uuid-existing".to_string(),
                    "id:uuid-luo".to_string(),
                    "id:uuid-ming".to_string(),
                ],
                "历史键改写为 UUID，已迁移的不受影响"
            );
        }


    }

    use super::*;
    use crate::features::command::CommandObservation;
    use miliastra_playback::{ProviderId, TrackKey, TrackMetadata, TrackRef};

    struct FakePlayback {
        current: Option<PlayableTrack>,
        played: Vec<String>,
    }

    impl PlaylistPlaybackPort for FakePlayback {
        fn current_track(&self) -> Option<PlayableTrack> {
            self.current.clone()
        }
        fn play(&mut self, track: &PlayableTrack) -> Result<()> {
            self.played.push(track.track_ref.key.to_string());
            Ok(())
        }
        fn import_playlist(
            &mut self,
            _provider: &str,
            _input: &str,
        ) -> Result<PlaylistImportOperation> {
            anyhow::bail!("playlist import is unavailable in this test port")
        }
        fn enqueue_next(&mut self, track: &PlayableTrack) -> Result<bool> {
            self.played.push(format!("queued:{}", track.track_ref.key));
            Ok(true)
        }
        fn start_playlist(
            &mut self,
            _owner: &str,
            tracks: Vec<PlayableTrack>,
        ) -> Result<PersonalPlaylistStart> {
            let order = tracks
                .iter()
                .map(|track| track.track_ref.key.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            self.played
                .push(format!("playlist:{}:{order}", tracks.len()));
            Ok(PersonalPlaylistStart::Started(tracks.len()))
        }
        fn stop_playlist(&mut self) -> Result<bool> {
            self.played.push("playlist:stopped".to_owned());
            Ok(true)
        }
    }

    fn track(id: &str) -> PlayableTrack {
        track_from(ProviderId::QqMusic, id, &format!("歌曲{id}"))
    }

    fn track_from(provider: ProviderId, id: &str, title: &str) -> PlayableTrack {
        PlayableTrack {
            track_ref: TrackRef {
                key: TrackKey::new(provider, id).expect("valid test key"),
                resolver_locator: None,
            },
            metadata: TrackMetadata {
                title: title.to_owned(),
                artists: vec!["测试歌手".into()],
                album: None,
                duration_ms: Some(180_000),
            },
        }
    }

    /// 把带 key 的回复按内置默认文案展开，供断言成品文本使用。
    ///
    /// 生产路径由组装层查用户覆盖后渲染；测试不接配置，所以这里等价于内置文案。
    fn texts(replies: Vec<PlaylistReply>) -> Vec<String> {
        replies.iter().map(PlaylistReply::render_default).collect()
    }

    fn app() -> PlaylistApplication {
        PlaylistApplication::from_connection(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn envelope(text: &str) -> CommandEnvelope {
        CommandEnvelope::new(
            text,
            "测试用户",
            "pink",
            text,
            CommandObservation::default(),
        )
        .unwrap()
    }

    fn parsed(text: &str) -> Option<PlaylistCommand> {
        PlaylistCommand::parse_chat(&envelope(text)).map(|matched| matched.command)
    }

    fn window_parsed(text: &str) -> Option<PlaylistCommand> {
        PlaylistCommand::parse_window_chat(&envelope(text)).map(|matched| matched.command)
    }

    fn play(
        app: &PlaylistApplication,
        owner: &str,
        command: &PlaylistCommand,
        port: &mut FakePlayback,
    ) -> Vec<String> {
        texts(
            app.execute(owner, command, port, Instant::now(), 1_700_000_000)
                .unwrap(),
        )
    }

    #[test]
    fn help_is_grouped_into_short_game_chat_messages_without_line_breaks() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        // 帮助是单条多行模板：业务层返回 1 条，拆条发生在发送路径。
        let raw = play(&app, "甲", &PlaylistCommand::Help, &mut port);
        assert_eq!(raw.len(), 1);
        let messages = crate::text::split_configured_lines(&raw[0]);
        assert_eq!(messages.len(), 4);
        assert!(
            messages
                .iter()
                .all(|message| !message.contains(['\n', '\r']))
        );
        assert_eq!(
            messages.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "歌单: @歌单 推荐5首歌; @随机歌单/@歌单随机 随机推荐",
                "收藏: @收藏 收藏当前歌曲; @取消收藏",
                "模式: @歌单模式/@播放歌单 进入歌单模式; @退出歌单模式/@大厅模式 退出歌单模式",
                "导入: @导入QQ歌单<数字ID>; @导入网易歌单<数字ID>; @歌单清空",
            ]
        );
        let help = messages.join(" ");
        for subcommand in [
            "@1",
            "@换一批",
            "@关闭",
            "@确认导入",
            "@取消导入",
            "@确认清空",
            "@放弃清空",
        ] {
            assert!(!help.contains(subcommand), "help should omit {subcommand}");
        }
    }

    #[test]
    fn orphaned_subcommands_are_silent_and_leave_playlist_unchanged() {
        let app = app();
        app.add("甲", &track("kept"), 1).unwrap();
        let mut port = FakePlayback {
            current: Some(track("current")),
            played: Vec::new(),
        };
        let commands = [
            PlaylistCommand::Select(1),
            PlaylistCommand::Reroll,
            PlaylistCommand::Close,
            PlaylistCommand::ConfirmImport,
            PlaylistCommand::CancelImport,
            PlaylistCommand::ConfirmClear,
            PlaylistCommand::AbortClear,
        ];

        for command in &commands {
            assert!(
                !app.has_command_context("甲", command, Instant::now())
                    .unwrap()
            );
            assert!(
                play(&app, "甲", command, &mut port).is_empty(),
                "{command:?}"
            );
        }
        assert_eq!(app.count("甲").unwrap(), 1);
        assert!(port.played.is_empty());
    }

    #[test]
    fn playlist_lock_keys_are_distinct_per_command() {
        let keys = [
            PlaylistCommand::Help.lock_key(),
            PlaylistCommand::Favorite.lock_key(),
            PlaylistCommand::Unfavorite.lock_key(),
            PlaylistCommand::PlayPlaylist.lock_key(),
            PlaylistCommand::HallMode.lock_key(),
            PlaylistCommand::Clear.lock_key(),
            PlaylistCommand::ConfirmClear.lock_key(),
            PlaylistCommand::Import {
                provider: "netease".to_string(),
                input: "1".to_string(),
            }
            .lock_key(),
            PlaylistCommand::Import {
                provider: "qqmusic".to_string(),
                input: "2".to_string(),
            }
            .lock_key(),
            PlaylistCommand::Select(1).lock_key(),
            PlaylistCommand::Select(2).lock_key(),
        ];
        let unique = keys.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn parses_commands_without_spaces() {
        assert_eq!(parsed("@收藏"), Some(PlaylistCommand::Favorite));
        assert_eq!(parsed("@取消收藏"), Some(PlaylistCommand::Unfavorite));
        assert_eq!(parsed("@歌单删除"), Some(PlaylistCommand::Unfavorite));
        assert_eq!(parsed("@歌单帮助"), Some(PlaylistCommand::Help));
        assert_eq!(parsed("@歌单随机"), Some(PlaylistCommand::Random));
        assert_eq!(parsed("@随机歌单"), Some(PlaylistCommand::Random));
        assert_eq!(parsed("@播放歌单"), Some(PlaylistCommand::PlayPlaylist));
        assert_eq!(parsed("@歌单模式"), Some(PlaylistCommand::PlayPlaylist));
        assert_eq!(parsed("@大厅模式"), Some(PlaylistCommand::HallMode));
        assert_eq!(parsed("@退出歌单模式"), Some(PlaylistCommand::HallMode));
        assert_eq!(parsed("@停止播放歌单"), Some(PlaylistCommand::StopPlaylist));
        assert_eq!(parsed("@歌单清空"), Some(PlaylistCommand::Clear));
        assert_eq!(parsed("@确认清空"), Some(PlaylistCommand::ConfirmClear));
        assert_eq!(parsed("@放弃清空"), Some(PlaylistCommand::AbortClear));
        assert_eq!(parsed("@歌单"), Some(PlaylistCommand::Open));
        assert_eq!(parsed("@换一批"), Some(PlaylistCommand::Reroll));
        assert_eq!(parsed("@关闭"), Some(PlaylistCommand::Close));
        assert_eq!(
            parsed("@导入QQ歌单8162013709"),
            Some(PlaylistCommand::Import {
                provider: "qqmusic".to_owned(),
                input: "8162013709".to_owned()
            })
        );
        assert_eq!(
            parsed("@导入网易歌单6787973700"),
            Some(PlaylistCommand::Import {
                provider: "netease".to_owned(),
                input: "6787973700".to_owned()
            })
        );
        // `导入网易云歌单` 是隐藏别名，不在帮助中展示。
        assert_eq!(
            parsed("@导入网易云歌单6787973700"),
            Some(PlaylistCommand::Import {
                provider: "netease".to_owned(),
                input: "6787973700".to_owned()
            })
        );
        assert_eq!(parsed("@确认导入"), Some(PlaylistCommand::ConfirmImport));
        assert_eq!(parsed("@取消导入"), Some(PlaylistCommand::CancelImport));
        // OCR 丢空格后仍可匹配。
        assert_eq!(parsed("@歌 单 随 机"), Some(PlaylistCommand::Random));
    }

    #[test]
    fn parses_window_selection_only_for_one_to_five() {
        for index in 1..=5 {
            let text = format!("@{index}");
            assert_eq!(parsed(&text), None, "ordinary parser must not claim {text}");
            assert_eq!(window_parsed(&text), Some(PlaylistCommand::Select(index)));
        }
        assert_eq!(window_parsed("@0"), None);
        assert_eq!(window_parsed("@6"), None);
        // 其它模块的命令不应被歌单认领。
        assert_eq!(parsed("@点歌 晴天"), None);
        assert_eq!(parsed("#塔罗牌"), None);
    }

    #[test]
    fn favorite_is_deduplicated_in_database() {
        let app = app();
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        assert!(play(&app, "甲", &PlaylistCommand::Favorite, &mut port)[0].contains("已收藏"));
        assert!(
            play(&app, "甲", &PlaylistCommand::Favorite, &mut port)[0].contains("已在你的歌单中")
        );
        assert_eq!(app.count("甲").unwrap(), 1);
        // 归属隔离：另一个昵称看不到。
        assert_eq!(app.count("乙").unwrap(), 0);
    }

    #[test]
    fn platform_provider_and_id_are_the_only_deduplication_key() {
        let app = app();
        let qq_one = track_from(ProviderId::QqMusic, "same-id", "同名歌曲");
        let qq_two = track_from(ProviderId::QqMusic, "other-id", "同名歌曲");
        let netease_same_numeric_id = track_from(ProviderId::Netease, "same-id", "同名歌曲");
        assert!(app.add("甲", &qq_one, 100).unwrap());
        assert!(app.add("甲", &qq_two, 101).unwrap());
        assert!(app.add("甲", &netease_same_numeric_id, 102).unwrap());
        assert_eq!(app.count("甲").unwrap(), 3);

        let refreshed = track_from(ProviderId::QqMusic, "same-id", "平台更新后的标题");
        assert!(!app.add("甲", &refreshed, 200).unwrap());
        let stored = app
            .items("甲")
            .unwrap()
            .into_iter()
            .find(|track| {
                track.track_ref.key.provider == ProviderId::QqMusic
                    && track.track_ref.key.id == "same-id"
            })
            .unwrap();
        assert_eq!(stored.metadata.title, "平台更新后的标题");
        let added_at: i64 = app
            .connection()
            .unwrap()
            .query_row(
                "SELECT added_at FROM playlist_items WHERE owner = ?1 AND track_key = ?2",
                params!["甲", qq_one.track_ref.key.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(added_at, 100);
    }

    #[test]
    fn batch_import_is_atomic_and_deduplicates_existing_and_repeated_keys() {
        let app = app();
        let (added, duplicates) = app
            .add_many("甲", &[track("a"), track("a"), track("b")], 1_700_000_000)
            .unwrap();
        assert_eq!((added, duplicates), (2, 1));
        assert_eq!(app.count("甲").unwrap(), 2);
        let (added, duplicates) = app
            .add_many(
                "甲",
                &[
                    track_from(ProviderId::QqMusic, "a", "歌曲a更新版"),
                    track("c"),
                ],
                1_700_000_001,
            )
            .unwrap();
        assert_eq!((added, duplicates), (1, 1));
        assert_eq!(app.count("甲").unwrap(), 3);
        assert_eq!(app.items("甲").unwrap()[0].metadata.title, "歌曲a更新版");
    }

    #[test]
    fn import_confirmation_is_owner_scoped_and_writes_all_tracks_together() {
        let app = app();
        let now = Instant::now();
        app.state.pending_imports.lock().unwrap().insert(
            "甲".to_owned(),
            PendingPlaylistImport::Ready {
                playlist: miliastra_playback::ImportedPlaylist {
                    provider: "qqmusic".to_owned(),
                    id: "8162013709".to_owned(),
                    title: "测试歌单".to_owned(),
                    creator: Some("创建者".to_owned()),
                    tracks: vec![track("a"), track("b")],
                },
                deadline: now + IMPORT_TTL,
            },
        );
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        let other_owner = texts(app
            .execute("乙", &PlaylistCommand::ConfirmImport, &mut port, now, 1)
            .unwrap());
        assert!(other_owner.is_empty());
        assert_eq!(app.count("甲").unwrap(), 0);
        assert!(
            app.has_command_context("甲", &PlaylistCommand::ConfirmImport, now)
                .unwrap()
        );
        assert!(
            !app.has_command_context("乙", &PlaylistCommand::ConfirmImport, now)
                .unwrap()
        );
        let owner = texts(app
            .execute("甲", &PlaylistCommand::ConfirmImport, &mut port, now, 1)
            .unwrap());
        assert!(owner[0].contains("新增2首"));
        assert_eq!(app.count("甲").unwrap(), 2);
        assert_eq!(app.count("乙").unwrap(), 0);
    }

    /// 模拟「歌单模式正激活且归属为甲」，用于验证清空会立即结束模式。
    struct ActiveModePlayback {
        stopped: bool,
    }

    impl PlaylistPlaybackPort for ActiveModePlayback {
        fn current_track(&self) -> Option<PlayableTrack> {
            None
        }
        fn play(&mut self, _: &PlayableTrack) -> Result<()> {
            anyhow::bail!("unused")
        }
        fn import_playlist(&mut self, _: &str, _: &str) -> Result<PlaylistImportOperation> {
            anyhow::bail!("unused")
        }
        fn playlist_mode_owner(&self) -> Option<String> {
            Some("甲".to_owned())
        }
        fn stop_playlist(&mut self) -> Result<bool> {
            self.stopped = true;
            Ok(true)
        }
    }

    #[test]
    fn clearing_favorites_exits_the_running_playlist_mode() {
        let app = app();
        let now = Instant::now();
        app.add("甲", &track("a"), 1).unwrap();
        let mut port = ActiveModePlayback { stopped: false };
        let prompt = texts(app
            .execute("甲", &PlaylistCommand::Clear, &mut port, now, 1)
            .unwrap());
        assert!(prompt[0].contains("确认清空"));
        let cleared = texts(app
            .execute("甲", &PlaylistCommand::ConfirmClear, &mut port, now, 1)
            .unwrap());
        assert!(cleared[0].contains("已清空"));
        assert!(
            cleared[0].contains("已退出"),
            "清空应立即结束运行中的歌单模式"
        );
        assert!(port.stopped);
    }

    /// 记录「并入运行中随机池」的调用，验证确认导入会通知播放侧。
    struct RecordingExtendPlayback {
        extended: Vec<String>,
    }

    impl PlaylistPlaybackPort for RecordingExtendPlayback {
        fn current_track(&self) -> Option<PlayableTrack> {
            None
        }
        fn play(&mut self, _: &PlayableTrack) -> Result<()> {
            anyhow::bail!("unused")
        }
        fn import_playlist(&mut self, _: &str, _: &str) -> Result<PlaylistImportOperation> {
            anyhow::bail!("unused")
        }
        fn extend_playlist_tracks(
            &mut self,
            owner: &str,
            tracks: &[PlayableTrack],
        ) -> Result<bool> {
            for track in tracks {
                self.extended
                    .push(format!("{owner}:{}", track.track_ref.key));
            }
            Ok(true)
        }
    }

    #[test]
    fn confirmed_import_extends_the_running_playlist_pool() {
        let app = app();
        let now = Instant::now();
        app.state.pending_imports.lock().unwrap().insert(
            "甲".to_owned(),
            PendingPlaylistImport::Ready {
                playlist: miliastra_playback::ImportedPlaylist {
                    provider: "qqmusic".to_owned(),
                    id: "8162013709".to_owned(),
                    title: "测试歌单".to_owned(),
                    creator: None,
                    tracks: vec![track("a"), track("b")],
                },
                deadline: now + IMPORT_TTL,
            },
        );
        let mut port = RecordingExtendPlayback {
            extended: Vec::new(),
        };
        let replies = texts(app
            .execute("甲", &PlaylistCommand::ConfirmImport, &mut port, now, 1)
            .unwrap());
        assert!(replies[0].contains("新增2首"));
        assert_eq!(port.extended.len(), 2, "确认导入应把新歌并入运行中的随机池");
    }

    #[test]
    fn favorite_without_playback_reports_clearly() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        assert!(
            play(&app, "甲", &PlaylistCommand::Favorite, &mut port)[0].contains("当前没有正在播放")
        );
    }

    #[test]
    fn unfavorite_removes_current_track() {
        let app = app();
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        play(&app, "甲", &PlaylistCommand::Favorite, &mut port);
        assert!(
            play(&app, "甲", &PlaylistCommand::Unfavorite, &mut port)[0].contains("已取消收藏")
        );
        assert_eq!(app.count("甲").unwrap(), 0);
        assert!(play(&app, "甲", &PlaylistCommand::Unfavorite, &mut port)[0].contains("没有"));
    }

    #[test]
    fn recommendation_sends_one_track_per_message_with_seven_for_five_tracks() {
        let tracks = (0..5)
            .map(|index| track(&index.to_string()))
            .collect::<Vec<_>>();
        let messages = texts(window_messages(&tracks));
        assert_eq!(messages.len(), 7);
        assert_eq!(messages[0], "为你推荐5首");
        for index in 1..=5 {
            assert!(messages[index].starts_with(&format!("{index}. ")));
        }
        assert!(messages[6].contains("加入下一曲"));
    }

    #[test]
    fn selection_enqueues_track_instead_of_replacing_current_playback() {
        let app = app();
        let selected = track("selected");
        app.store_window(
            "甲",
            vec![selected.clone()],
            vec![selected.track_ref.key.to_string()],
            Instant::now(),
            false,
        )
        .unwrap();
        let mut port = FakePlayback {
            current: Some(track("current")),
            played: Vec::new(),
        };
        let replies = play(&app, "甲", &PlaylistCommand::Select(1), &mut port);
        assert!(replies[0].contains("下一曲"));
        assert_eq!(
            port.played,
            vec![format!("queued:{}", selected.track_ref.key)]
        );
        assert_eq!(port.current.unwrap().track_ref.key.id, "current");
    }

    #[test]
    fn playlist_playback_starts_random_queue_and_can_be_stopped() {
        let app = app();
        let tracks = (0..8)
            .map(|index| track(&format!("t{index}")))
            .collect::<Vec<_>>();
        app.add_many("甲", &tracks, 1).unwrap();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        let started = play(&app, "甲", &PlaylistCommand::PlayPlaylist, &mut port);
        assert!(started[0].contains("已进入歌单模式"));
        assert!(started[0].contains("随机顺序"));
        let queued = port.played[0]
            .strip_prefix("playlist:8:")
            .unwrap()
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let expected = (0..8).map(|index| format!("t{index}")).collect::<Vec<_>>();
        let mut sorted = queued.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, expected);
        assert_ne!(queued, expected);
        play(&app, "甲", &PlaylistCommand::StopPlaylist, &mut port);
        assert_eq!(port.played[1], "playlist:stopped");
    }

    /// 仅验证「已在歌单模式」分支的文案映射；真实幂等由 PlaybackApplication 保证。
    struct AlreadyActivePlayback;

    impl PlaylistPlaybackPort for AlreadyActivePlayback {
        fn current_track(&self) -> Option<PlayableTrack> {
            None
        }
        fn play(&mut self, _: &PlayableTrack) -> Result<()> {
            anyhow::bail!("unused")
        }
        fn import_playlist(&mut self, _: &str, _: &str) -> Result<PlaylistImportOperation> {
            anyhow::bail!("unused")
        }
        fn start_playlist(
            &mut self,
            _owner: &str,
            _tracks: Vec<PlayableTrack>,
        ) -> Result<PersonalPlaylistStart> {
            Ok(PersonalPlaylistStart::AlreadyActive {
                owner: "甲".to_string(),
            })
        }
    }

    #[test]
    fn playlist_mode_command_reports_the_current_owner_when_already_active() {
        let app = app();
        let tracks = (0..4)
            .map(|index| track(&format!("t{index}")))
            .collect::<Vec<_>>();
        app.add_many("乙", &tracks, 1).unwrap();
        let mut port = AlreadyActivePlayback;
        let replies = texts(app
            .execute(
                "乙",
                &PlaylistCommand::PlayPlaylist,
                &mut port,
                Instant::now(),
                1_700_000_000,
            )
            .unwrap());
        assert_eq!(
            replies,
            vec!["当前为甲的歌单;如需切换请先发@大厅模式再发@歌单模式".to_string()]
        );
    }

    #[test]
    fn random_recommends_one_song_without_starting_playback() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        assert!(play(&app, "甲", &PlaylistCommand::Random, &mut port)[0].contains("还是空的"));
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        play(&app, "甲", &PlaylistCommand::Favorite, &mut port);
        let recommendation = play(&app, "甲", &PlaylistCommand::Random, &mut port);
        assert!(recommendation[0].contains("随机推荐"));
        assert!(port.played.is_empty());
    }

    #[test]
    fn clear_needs_matching_confirmation_and_abort_keeps_items() {
        let app = app();
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        play(&app, "甲", &PlaylistCommand::Favorite, &mut port);
        assert!(play(&app, "甲", &PlaylistCommand::ConfirmClear, &mut port).is_empty());
        assert!(play(&app, "甲", &PlaylistCommand::AbortClear, &mut port).is_empty());
        assert!(play(&app, "甲", &PlaylistCommand::Clear, &mut port)[0].contains("将清空"));
        assert_eq!(app.count("甲").unwrap(), 1, "发起清空不应立即删除");
        assert!(
            play(&app, "甲", &PlaylistCommand::AbortClear, &mut port)[0].contains("已取消清空")
        );
        assert_eq!(app.count("甲").unwrap(), 1);
        play(&app, "甲", &PlaylistCommand::Clear, &mut port);
        assert!(play(&app, "甲", &PlaylistCommand::ConfirmClear, &mut port)[0].contains("已清空"));
        assert_eq!(app.count("甲").unwrap(), 0);
    }

    #[test]
    fn clear_confirmation_expires() {
        let app = app();
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        play(&app, "甲", &PlaylistCommand::Favorite, &mut port);
        let started = Instant::now();
        app.execute("甲", &PlaylistCommand::Clear, &mut port, started, 0)
            .unwrap();
        let later = started + CLEAR_CONFIRM_TTL + Duration::from_secs(1);
        let reply = texts(app
            .execute("甲", &PlaylistCommand::ConfirmClear, &mut port, later, 0)
            .unwrap());
        assert!(reply.is_empty());
        assert_eq!(app.count("甲").unwrap(), 1);
    }

    #[test]
    fn window_selects_by_index_and_plays() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        for id in ["a", "b", "c"] {
            app.add("甲", &track(id), 0).unwrap();
        }
        let now = Instant::now();
        let opened = texts(app
            .execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap());
        assert_eq!(opened.len(), 5);
        assert!(opened[0].starts_with("为你推荐3首"));
        assert!(opened[1].starts_with("1. "));
        assert!(opened[4].contains("20秒内回复@1-@3加入下一曲"));
        let reply = texts(app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, now, 7)
            .unwrap());
        assert!(reply[0].contains("下一曲"), "{reply:?}");
        assert_eq!(port.played.len(), 1);
        assert!(port.played[0].starts_with("queued:"));
        // 选过之后窗口关闭。
        let again = texts(app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, now, 7)
            .unwrap());
        assert!(again.is_empty());
    }

    #[test]
    fn recommendation_gets_full_ttl_after_messages_finish_sending() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        app.add("甲", &track("a"), 0).unwrap();
        let started = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, started, 7)
            .unwrap();
        let messages_sent_at = started + Duration::from_secs(8);
        let deadline = messages_sent_at + WINDOW_TTL;
        app.set_window_deadline("甲", deadline).unwrap();
        assert_eq!(
            app.window_len(
                "甲",
                messages_sent_at + WINDOW_TTL - Duration::from_millis(1)
            )
            .unwrap(),
            1
        );
        assert_eq!(app.window_len("甲", deadline).unwrap(), 0);
    }

    #[test]
    fn window_expires_after_ttl() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        app.add("甲", &track("a"), 0).unwrap();
        let started = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, started, 7)
            .unwrap();
        let later = started + WINDOW_TTL + Duration::from_secs(1);
        let reply = texts(app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, later, 7)
            .unwrap());
        assert!(reply.is_empty());
        assert!(port.played.is_empty(), "过期窗口不应触发播放");
    }

    #[test]
    fn window_reroll_excludes_shown_tracks() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        for index in 0..12 {
            app.add("甲", &track(&format!("t{index:02}")), index)
                .unwrap();
        }
        let now = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap();
        let first = app.window_shown("甲", now).unwrap().unwrap();
        assert_eq!(first.len(), WINDOW_CANDIDATES);
        app.execute("甲", &PlaylistCommand::Reroll, &mut port, now, 8)
            .unwrap();
        let accumulated = app.window_shown("甲", now).unwrap().unwrap();
        // 换一批给的必须是全新曲目；若重复展示旧曲，累计集合会短于 2×候选数。
        assert_eq!(
            accumulated.len(),
            WINDOW_CANDIDATES * 2,
            "换一批应带来 {} 首全新曲目",
            WINDOW_CANDIDATES
        );
        let fresh: Vec<_> = accumulated
            .iter()
            .filter(|key| !first.contains(key))
            .collect();
        assert_eq!(fresh.len(), WINDOW_CANDIDATES);
    }

    #[test]
    fn eight_tracks_reroll_as_five_three_then_new_five() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        for index in 0..8 {
            app.add("甲", &track(&format!("t{index}")), index).unwrap();
        }
        let now = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap();
        let first = app.window_shown("甲", now).unwrap().unwrap();
        assert_eq!(app.window_len("甲", now).unwrap(), 5);
        app.execute("甲", &PlaylistCommand::Reroll, &mut port, now, 8)
            .unwrap();
        assert_eq!(app.window_len("甲", now).unwrap(), 3);
        let shown = app.window_shown("甲", now).unwrap().unwrap();
        assert_eq!(shown.len(), 8);
        let windows = app.state.windows.lock().unwrap();
        assert!(
            windows["甲"]
                .tracks
                .iter()
                .all(|track| !first.contains(&track.track_ref.key.to_string()))
        );
        drop(windows);
        app.execute("甲", &PlaylistCommand::Reroll, &mut port, now, 9)
            .unwrap();
        assert_eq!(app.window_len("甲", now).unwrap(), 5);
        assert_eq!(app.window_shown("甲", now).unwrap().unwrap().len(), 5);
    }

    #[test]
    fn fewer_than_five_shows_all_and_restarts_without_accumulating_old_cycle() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        for id in ["a", "b", "c"] {
            app.add("甲", &track(id), 0).unwrap();
        }
        let now = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap();
        let first = app.window_shown("甲", now).unwrap().unwrap();
        assert_eq!(first.len(), 3);
        for seed in [8, 9] {
            app.execute("甲", &PlaylistCommand::Reroll, &mut port, now, seed)
                .unwrap();
            let shown = app.window_shown("甲", now).unwrap().unwrap();
            assert_eq!(app.window_len("甲", now).unwrap(), 3);
            assert_eq!(shown.len(), 3);
            assert!(shown.iter().all(|key| first.contains(key)));
        }
    }

    #[test]
    fn reroll_resets_deadline_and_other_owner_cannot_select() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        app.add("甲", &track("a"), 0).unwrap();
        let now = Instant::now();
        app.execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap();
        app.execute("乙", &PlaylistCommand::Select(1), &mut port, now, 7)
            .unwrap();
        assert!(port.played.is_empty());
        assert_eq!(app.window_len("甲", now).unwrap(), 1);
        let rerolled_at = now + Duration::from_secs(19);
        app.execute("甲", &PlaylistCommand::Reroll, &mut port, rerolled_at, 8)
            .unwrap();
        let reset_deadline = rerolled_at + WINDOW_TTL;
        assert_eq!(app.window_len("甲", now + WINDOW_TTL).unwrap(), 1);
        assert_eq!(app.window_len("甲", reset_deadline).unwrap(), 0);
        assert_eq!(
            app.state.windows.lock().unwrap()["甲"].deadline,
            reset_deadline
        );
    }

    #[test]
    fn window_close_is_idempotent() {
        let app = app();
        let mut port = FakePlayback {
            current: None,
            played: Vec::new(),
        };
        app.add("甲", &track("a"), 0).unwrap();
        assert!(play(&app, "甲", &PlaylistCommand::Close, &mut port).is_empty());
        play(&app, "甲", &PlaylistCommand::Open, &mut port);
        assert!(play(&app, "甲", &PlaylistCommand::Close, &mut port)[0].contains("已关闭"));
        assert!(play(&app, "甲", &PlaylistCommand::Close, &mut port).is_empty());
    }
}
