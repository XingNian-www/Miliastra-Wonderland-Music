//! 基于昵称归属的个人歌单收藏。
//!
//! 归属 owner = OCR 读到的昵称经 \`normalize_comparison_text\` 归一化后的结果，由调用方传入。
//! 存储与 tarot 一样复用统一数据库 \`playback.sqlite3\`，只新增自己的表，不改全局 schema 版本。
//! 采集阶段不在歌单内部去重；数据库层用 \`(owner, track_key)\` 主键去重，重复导入/收藏不会产生重复行。
use super::command::{CommandEnvelope, CommandPrefix, FeatureCommandMatch};
use crate::features::chat_text::compact_command;
use crate::text::{MAX_CHAT_WIDTH, display_width, split_numbered_chat_message};
use anyhow::{Result, anyhow};
use miliastra_playback::PlayableTrack;
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
    /// 从自己的收藏随机播放一首。
    Random,
    /// 请求清空，进入二次确认。
    Clear,
    /// 确认清空。
    ConfirmClear,
    /// 放弃清空。
    AbortClear,
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
        // 窗口数字选择：@1~@5（command_text 不含 @）。
        if let Ok(index) = text.parse::<u8>() {
            if (1..=WINDOW_CANDIDATES as u8).contains(&index) {
                return Some(FeatureCommandMatch::new(
                    text,
                    envelope.command_text(),
                    Self::Select(index),
                ));
            }
        }
        let command = match text.as_str() {
            "歌单" => Self::Open,
            "换一批" => Self::Reroll,
            "关闭" => Self::Close,
            "歌单帮助" => Self::Help,
            "收藏" => Self::Favorite,
            "取消收藏" | "歌单删除" => Self::Unfavorite,
            "歌单随机" | "随机歌单" => Self::Random,
            "歌单清空" => Self::Clear,
            "确认清空" => Self::ConfirmClear,
            "放弃清空" => Self::AbortClear,
            _ => return None,
        };
        Some(FeatureCommandMatch::new(
            text,
            envelope.command_text(),
            command,
        ))
    }

    pub(crate) fn lock_key(&self) -> String {
        "playlist".into()
    }
}

/// 收藏能力的播放侧依赖。抽成端口以便单测不依赖真实播放器。
pub(crate) trait PlaylistPlaybackPort {
    /// 当前正在播放的曲目；未播放时为 None。
    fn current_track(&self) -> Option<PlayableTrack>;
    /// 播放一首曲目。
    fn play(&mut self, track: &PlayableTrack) -> Result<()>;
}

/// 歌单窗口：展示候选、记录已展示集合与截止时间。
struct PlaylistWindow {
    tracks: Vec<PlayableTrack>,
    shown: Vec<String>,
    deadline: Instant,
}

struct PlaylistState {
    connection: Mutex<Connection>,
    /// 每个 owner 的清空确认截止时间。
    pending_clear: Mutex<HashMap<String, Instant>>,
    /// 每个 owner 的当前歌单窗口。
    windows: Mutex<HashMap<String, PlaylistWindow>>,
}

#[derive(Clone)]
pub(crate) struct PlaylistApplication {
    state: Arc<PlaylistState>,
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
        Ok(Self {
            state: Arc::new(PlaylistState {
                connection: Mutex::new(connection),
                pending_clear: Mutex::new(HashMap::new()),
                windows: Mutex::new(HashMap::new()),
            }),
        })
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.state
            .connection
            .lock()
            .map_err(|_| anyhow!("歌单存储锁已中毒"))
    }

    /// 收藏一首曲目。返回 true 表示新增，false 表示已存在（数据库层去重）。
    pub(crate) fn add(&self, owner: &str, track: &PlayableTrack, now_epoch: i64) -> Result<bool> {
        let track_key = track.track_ref.key.to_string();
        let track_json = serde_json::to_string(track)?;
        let connection = self.connection()?;
        let changed = connection.execute(
            "INSERT INTO playlist_items(owner, track_key, track_json, added_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(owner, track_key) DO NOTHING",
            params![owner, track_key, track_json, now_epoch],
        )?;
        Ok(changed > 0)
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
    ) -> Result<()> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        windows.retain(|_, window| window.deadline > now);
        let deadline = windows
            .get(owner)
            .map_or(now + WINDOW_TTL, |window| window.deadline);
        windows.insert(
            owner.to_owned(),
            PlaylistWindow {
                tracks,
                shown,
                // 重抽保持本窗口的总期限，不因换一批而无限延期。
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

    pub(crate) fn close_window(&self, owner: &str) -> Result<bool> {
        let mut windows = self
            .state
            .windows
            .lock()
            .map_err(|_| anyhow!("歌单窗口状态不可用"))?;
        Ok(windows.remove(owner).is_some())
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

    fn drop_pending_clear(&self, owner: &str) -> Result<()> {
        let mut pending = self
            .state
            .pending_clear
            .lock()
            .map_err(|_| anyhow!("歌单确认状态不可用"))?;
        pending.remove(owner);
        Ok(())
    }

    pub(crate) fn execute(
        &self,
        owner: &str,
        command: &PlaylistCommand,
        playback: &mut dyn PlaylistPlaybackPort,
        now: Instant,
        now_epoch: i64,
    ) -> Result<Vec<String>> {
        let owner = owner.trim();
        if owner.is_empty() {
            return Ok(vec!["歌单需要先识别发言者，请稍后重试。".into()]);
        }
        match command {
            PlaylistCommand::Help => Ok(fit_messages(
                "歌单",
                "@歌单 查看收藏并随机推荐；@歌单随机 随机播放一首；@收藏 收藏当前播放；@取消收藏 取消收藏当前播放；@歌单清空 清空收藏（需确认）。",
            )),
            PlaylistCommand::Open => {
                self.close_window(owner)?;
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec!["你的歌单还是空的，先播一首再说 @收藏 吧。".into()]);
                }
                let seed = seed_from(now_epoch, owner);
                let tracks = self.pick_many(owner, &[], WINDOW_CANDIDATES, seed)?;
                let shown: Vec<String> = tracks
                    .iter()
                    .map(|track| track.track_ref.key.to_string())
                    .collect();
                self.store_window(owner, tracks.clone(), shown, now)?;
                Ok(fit_messages("歌单", &window_text("歌单推荐", &tracks)))
            }
            PlaylistCommand::Select(index) => {
                let index = (*index as usize).saturating_sub(1);
                match self.take_window_selection(owner, index, now)? {
                    Some(track) => {
                        playback.play(&track)?;
                        Ok(vec![format!("开始播放《{}》。", track_title(&track))])
                    }
                    None => {
                        let len = self.window_len(owner, now)?;
                        if len == 0 {
                            Ok(vec!["没有可选的歌单窗口，请先发送 @歌单。".into()])
                        } else {
                            Ok(vec![format!("请回复 @1~@{len} 选择，@关闭 退出。")])
                        }
                    }
                }
            }
            PlaylistCommand::Reroll => {
                let Some(shown) = self.window_shown(owner, now)? else {
                    return Ok(vec!["没有可选的歌单窗口，请先发送 @歌单。".into()]);
                };
                let seed = seed_from(now_epoch ^ 0x5DEE_CE66, owner);
                let tracks = self.pick_many(owner, &shown, WINDOW_CANDIDATES, seed)?;
                if tracks.is_empty() {
                    return Ok(vec!["没有更多可以换的歌了，回复 @关闭 退出。".into()]);
                }
                // pick_many 只有未展示池耗尽才回到全部曲目，此时重置周期。
                let restarted = tracks
                    .iter()
                    .all(|track| shown.contains(&track.track_ref.key.to_string()));
                let mut all_shown = if restarted { Vec::new() } else { shown };
                all_shown.extend(tracks.iter().map(|track| track.track_ref.key.to_string()));
                self.store_window(owner, tracks.clone(), all_shown, now)?;
                Ok(fit_messages("歌单", &window_text("换一批", &tracks)))
            }
            PlaylistCommand::Close => {
                if self.close_window(owner)? {
                    Ok(vec!["已关闭歌单窗口。".into()])
                } else {
                    Ok(vec!["当前没有打开的歌单窗口。".into()])
                }
            }
            PlaylistCommand::Favorite => {
                let Some(track) = playback.current_track() else {
                    return Ok(vec!["当前没有正在播放的歌曲，无法收藏。".into()]);
                };
                let title = track_title(&track);
                if self.add(owner, &track, now_epoch)? {
                    Ok(vec![format!(
                        "已收藏《{title}》，歌单现有 {} 首。",
                        self.count(owner)?
                    )])
                } else {
                    Ok(vec![format!("《{title}》已经在你的歌单里了。")])
                }
            }
            PlaylistCommand::Unfavorite => {
                let Some(track) = playback.current_track() else {
                    return Ok(vec!["当前没有正在播放的歌曲，无法取消收藏。".into()]);
                };
                let title = track_title(&track);
                if self.remove(owner, &track)? {
                    Ok(vec![format!(
                        "已从歌单移除《{title}》，剩余 {} 首。",
                        self.count(owner)?
                    )])
                } else {
                    Ok(vec![format!("《{title}》不在你的歌单里。")])
                }
            }
            PlaylistCommand::Random => {
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec!["你的歌单还是空的，先播一首再说 @收藏 吧。".into()]);
                }
                let seed = seed_from(now_epoch, owner);
                let Some(track) = self.pick_random(owner, &[], seed)? else {
                    return Ok(vec!["你的歌单还是空的。".into()]);
                };
                playback.play(&track)?;
                Ok(vec![format!(
                    "随机播放《{}》（共 {total} 首）。",
                    track_title(&track)
                )])
            }
            PlaylistCommand::Clear => {
                let total = self.count(owner)?;
                if total == 0 {
                    return Ok(vec!["你的歌单本来就是空的。".into()]);
                }
                self.mark_pending_clear(owner, now)?;
                Ok(vec![format!(
                    "将清空你的 {total} 首收藏，回复 @确认清空 执行，回复 @放弃清空 取消。"
                )])
            }
            PlaylistCommand::ConfirmClear => {
                if !self.take_pending_clear(owner, now)? {
                    return Ok(vec!["没有待确认的清空请求，请先发送 @歌单清空。".into()]);
                }
                let removed = self.clear(owner)?;
                Ok(vec![format!("已清空你的歌单，共移除 {removed} 首。")])
            }
            PlaylistCommand::AbortClear => {
                self.drop_pending_clear(owner)?;
                Ok(vec!["已取消清空。".into()])
            }
        }
    }
}

/// 组装窗口展示文案：序号 + 曲名 + 操作提示。
fn window_text(prefix: &str, tracks: &[PlayableTrack]) -> String {
    let list = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| format!("{}、{}", index + 1, track_title(track)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{prefix}：「{list}」。回复@1-@{}选择，@换一批重抽，@关闭退出。",
        tracks.len()
    )
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

fn fit_messages(label: &str, text: &str) -> Vec<String> {
    if display_width(text) <= MAX_CHAT_WIDTH {
        return vec![text.to_owned()];
    }
    split_numbered_chat_message(label, text)
}

#[cfg(test)]
mod tests {
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
    }

    fn track(id: &str) -> PlayableTrack {
        PlayableTrack {
            track_ref: TrackRef {
                key: TrackKey::new(ProviderId::QqMusic, id).expect("valid test key"),
                resolver_locator: None,
            },
            metadata: TrackMetadata {
                title: format!("歌曲{id}"),
                artists: vec!["测试歌手".into()],
                album: None,
                duration_ms: Some(180_000),
            },
        }
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

    fn play(
        app: &PlaylistApplication,
        owner: &str,
        command: &PlaylistCommand,
        port: &mut FakePlayback,
    ) -> Vec<String> {
        app.execute(owner, command, port, Instant::now(), 1_700_000_000)
            .unwrap()
    }

    #[test]
    fn parses_commands_without_spaces() {
        assert_eq!(parsed("@收藏"), Some(PlaylistCommand::Favorite));
        assert_eq!(parsed("@取消收藏"), Some(PlaylistCommand::Unfavorite));
        assert_eq!(parsed("@歌单删除"), Some(PlaylistCommand::Unfavorite));
        assert_eq!(parsed("@歌单帮助"), Some(PlaylistCommand::Help));
        assert_eq!(parsed("@歌单随机"), Some(PlaylistCommand::Random));
        assert_eq!(parsed("@随机歌单"), Some(PlaylistCommand::Random));
        assert_eq!(parsed("@歌单清空"), Some(PlaylistCommand::Clear));
        assert_eq!(parsed("@确认清空"), Some(PlaylistCommand::ConfirmClear));
        assert_eq!(parsed("@放弃清空"), Some(PlaylistCommand::AbortClear));
        assert_eq!(parsed("@歌单"), Some(PlaylistCommand::Open));
        assert_eq!(parsed("@换一批"), Some(PlaylistCommand::Reroll));
        assert_eq!(parsed("@关闭"), Some(PlaylistCommand::Close));
        // OCR 丢空格后仍可匹配。
        assert_eq!(parsed("@歌 单 随 机"), Some(PlaylistCommand::Random));
    }

    #[test]
    fn parses_window_selection_only_for_one_to_five() {
        assert_eq!(parsed("@1"), Some(PlaylistCommand::Select(1)));
        assert_eq!(parsed("@5"), Some(PlaylistCommand::Select(5)));
        assert_eq!(parsed("@0"), None);
        assert_eq!(parsed("@6"), None);
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
        assert!(play(&app, "甲", &PlaylistCommand::Favorite, &mut port)[0].contains("已经在"));
        assert_eq!(app.count("甲").unwrap(), 1);
        // 归属隔离：另一个昵称看不到。
        assert_eq!(app.count("乙").unwrap(), 0);
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
            play(&app, "甲", &PlaylistCommand::Unfavorite, &mut port)[0].contains("已从歌单移除")
        );
        assert_eq!(app.count("甲").unwrap(), 0);
        assert!(
            play(&app, "甲", &PlaylistCommand::Unfavorite, &mut port)[0].contains("不在你的歌单")
        );
    }

    #[test]
    fn random_plays_and_reports_empty() {
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
        assert!(play(&app, "甲", &PlaylistCommand::Random, &mut port)[0].contains("随机播放"));
        assert_eq!(port.played.len(), 1);
    }

    #[test]
    fn clear_needs_matching_confirmation_and_abort_keeps_items() {
        let app = app();
        let mut port = FakePlayback {
            current: Some(track("a")),
            played: Vec::new(),
        };
        play(&app, "甲", &PlaylistCommand::Favorite, &mut port);
        assert!(
            play(&app, "甲", &PlaylistCommand::ConfirmClear, &mut port)[0].contains("没有待确认")
        );
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
        let reply = app
            .execute("甲", &PlaylistCommand::ConfirmClear, &mut port, later, 0)
            .unwrap();
        assert!(reply[0].contains("没有待确认"));
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
        let opened = app
            .execute("甲", &PlaylistCommand::Open, &mut port, now, 7)
            .unwrap();
        assert!(opened[0].contains("歌单推荐"));
        let reply = app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, now, 7)
            .unwrap();
        assert!(reply[0].contains("开始播放"), "{reply:?}");
        assert_eq!(port.played.len(), 1);
        // 选过之后窗口关闭。
        let again = app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, now, 7)
            .unwrap();
        assert!(again[0].contains("没有可选的歌单窗口"));
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
        let reply = app
            .execute("甲", &PlaylistCommand::Select(1), &mut port, later, 7)
            .unwrap();
        assert!(reply[0].contains("没有可选的歌单窗口"));
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
    fn reroll_preserves_total_deadline_and_other_owner_cannot_select() {
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
        app.execute(
            "甲",
            &PlaylistCommand::Reroll,
            &mut port,
            now + Duration::from_secs(19),
            8,
        )
        .unwrap();
        assert_eq!(app.window_len("甲", now + WINDOW_TTL).unwrap(), 0);
        assert_eq!(
            app.state.windows.lock().unwrap()["甲"].deadline,
            now + WINDOW_TTL
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
        assert!(play(&app, "甲", &PlaylistCommand::Close, &mut port)[0].contains("没有打开"));
        play(&app, "甲", &PlaylistCommand::Open, &mut port);
        assert!(play(&app, "甲", &PlaylistCommand::Close, &mut port)[0].contains("已关闭"));
    }
}
