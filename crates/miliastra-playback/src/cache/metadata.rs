//! 缓存元数据存储：用 SQLite 持久化「哪些曲目被缓存过、是否完整、元数据、
//! 歌词」等结构信息，供缓存对账、状态统计与缓存歌曲列表查询使用。
//!
//! 与音频本体分离：音频数据仍是磁盘上的 `.part`/`.complete` 文件，
//! 歌词正文直接存入 SQLite（`cached_lyrics.lyrics_json`），本模块记录
//! 音频文件索引与歌词正文（身份、大小、时间、完成状态）。数据库固定位于
//! `<metadata_directory>/playback.sqlite3`。
//!
//! 安全边界：表结构里没有音源 URL、请求头、账号凭据等列，本模块也绝不
//! 写入这些内容；[`StreamSource`] 这类含 URL/headers 的类型不会出现在
//! 任何 API 中。
//!
//! 曲目身份统一使用 [`super::cache_key_hash`] 生成的 md5 哈希（source:id），
//! 与音频缓存文件名使用的哈希一致，保证两套状态可按 hash 对齐。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::SongKey;
use crate::model::TrackMetadata;

use super::cache_key_hash;

/// 元数据数据库文件名（固定于 metadata_directory 内）。
pub(super) const DATABASE_FILE_NAME: &str = "playback.sqlite3";

/// 当前 schema 版本（写入 PRAGMA user_version）。
const SCHEMA_VERSION: i64 = 1;

/// SQLite busy_timeout（毫秒）：并发访问数据库时等待锁的最长时间。
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// 建表语句。artists 以 JSON 数组文本存储；cached_lyrics 直接保存歌词
/// 正文（lyrics_json 与字节数、更新时间），外键级联到 cached_tracks，
/// 删除曲目记录时歌词一并删除，避免孤儿行。cached_tracks 的 source/id
/// 允许 NULL：对账发现磁盘孤儿 complete 文件（尚无身份信息）时也能登记。
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS cached_tracks (
    hash        TEXT PRIMARY KEY,
    source      TEXT,
    id          TEXT,
    title       TEXT,
    artists     TEXT NOT NULL DEFAULT '[]',
    album       TEXT,
    duration_ms INTEGER,
    complete    INTEGER NOT NULL DEFAULT 0,
    bytes       INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    play_count INTEGER NOT NULL DEFAULT 0 CHECK (play_count >= 0),
    requested_play_count INTEGER NOT NULL DEFAULT 0 CHECK (requested_play_count >= 0),
    pool_play_count INTEGER NOT NULL DEFAULT 0 CHECK (pool_play_count >= 0),
    cache_hit_count INTEGER NOT NULL DEFAULT 0 CHECK (cache_hit_count >= 0),
    failure_count INTEGER NOT NULL DEFAULT 0 CHECK (failure_count >= 0),
    last_played_at_ms INTEGER,
    last_failure_code TEXT,
    downloaded_at_ms INTEGER,
    last_play_generation INTEGER NOT NULL DEFAULT 0,
    last_play_session TEXT,
    last_failure_generation INTEGER NOT NULL DEFAULT 0,
    last_failure_session TEXT,
    UNIQUE (source, id)
);
CREATE INDEX IF NOT EXISTS idx_cached_tracks_updated_at
    ON cached_tracks (updated_at DESC);
CREATE TABLE IF NOT EXISTS cached_lyrics (
    hash        TEXT PRIMARY KEY
                REFERENCES cached_tracks(hash) ON DELETE CASCADE,
    lyrics_json BLOB NOT NULL,
    bytes       INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);
";

/// 曲目记录查询统一使用的列清单。
const TRACK_COLUMNS: &str = "hash, source, id, title, artists, album, duration_ms, complete, bytes, created_at, updated_at, play_count, requested_play_count, pool_play_count, cache_hit_count, failure_count, last_played_at_ms, last_failure_code, downloaded_at_ms";

/// 缓存曲目记录（对账与分页列表的返回载体）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TrackRecord {
    pub hash: String,
    /// 来源与 ID；对账登记的无身份孤儿完整文件为 None。
    pub source: Option<String>,
    pub id: Option<String>,
    /// 未记录元数据时为 None。
    pub metadata: Option<TrackMetadata>,
    /// 音频完整文件是否已落盘。
    pub complete: bool,
    /// 完整文件的字节数（未完整时为 0）。
    pub bytes: u64,
    pub created_at_epoch_secs: i64,
    pub updated_at_epoch_secs: i64,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
    pub last_played_at_ms: Option<u64>,
    pub last_failure_code: Option<String>,
    pub downloaded_at_ms: Option<u64>,
}

/// 指定曲目在元数据存储中的状态（[`MetadataStore::stats`] 返回）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TrackStatus {
    pub key: SongKey,
    /// 存储中是否有该曲目的记录。
    pub recorded: bool,
    pub complete: bool,
    pub bytes: Option<u64>,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
    pub last_played_at_ms: Option<u64>,
    pub last_failure_code: Option<String>,
}

/// 元数据存储整体统计（[`MetadataStore::stats`] 返回）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MetadataStats {
    pub total_tracks: usize,
    pub complete_tracks: usize,
    pub complete_bytes: u64,
    pub lyrics_tracks: usize,
    pub lyrics_bytes: u64,
    pub play_count: u64,
    pub requested_play_count: u64,
    pub pool_play_count: u64,
    pub cache_hit_count: u64,
    pub failure_count: u64,
}

/// SQLite 元数据存储：包装一个连接。
///
/// `Connection` 内部自带互斥，方法可用 `&self` 调用；但连接本身不是
/// `Sync`，需要跨线程共享时由调用方包一层 `Mutex`。
#[derive(Debug)]
pub(super) struct MetadataStore {
    conn: Connection,
}

impl MetadataStore {
    /// 打开（不存在则创建）`directory/playback.sqlite3` 并完成初始化：
    /// 创建目录、WAL 日志、synchronous=NORMAL、外键约束、busy_timeout，
    /// 建表并把 user_version 置为 1。
    pub(super) fn open(directory: &Path) -> Result<Self, MetadataStoreError> {
        std::fs::create_dir_all(directory)?;
        let conn = Connection::open(directory.join(DATABASE_FILE_NAME))?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// 按 hash upsert 曲目身份与可选元数据。
    ///
    /// - 首次出现：写入身份（source/id）、created_at 与（若有）元数据。
    /// - 已存在：刷新身份与 updated_at（作为最近使用信号）；
    ///   `metadata` 为 `Some` 时覆盖元数据字段，为 `None` 时保留已有元数据。
    pub(super) fn upsert_track(
        &self,
        key: &SongKey,
        metadata: Option<&TrackMetadata>,
    ) -> Result<(), MetadataStoreError> {
        let now = now_epoch_secs();
        // metadata 为 None 时各字段传 NULL/空串，靠 SQL 的 COALESCE 与
        // CASE 分支保留旧值；title 是否为 NULL 兼任「本次是否带元数据」标记。
        let (title, artists_json, album, duration_ms) = match metadata {
            Some(meta) => (
                Some(meta.title.as_str()),
                serde_json::to_string(&meta.artists)?,
                meta.album.as_deref(),
                meta.duration_ms.map(|ms| ms as i64),
            ),
            None => (None, String::new(), None, None),
        };
        self.conn.execute(
            "INSERT INTO cached_tracks
                (hash, source, id, title, artists, album, duration_ms, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(hash) DO UPDATE SET
                source = excluded.source,
                id = excluded.id,
                title = COALESCE(excluded.title, cached_tracks.title),
                artists = CASE WHEN excluded.title IS NULL
                               THEN cached_tracks.artists
                               ELSE excluded.artists END,
                album = COALESCE(excluded.album, cached_tracks.album),
                duration_ms = COALESCE(excluded.duration_ms, cached_tracks.duration_ms),
                updated_at = excluded.updated_at",
            params![
                cache_key_hash(key),
                key.source,
                key.id,
                title,
                artists_json,
                album,
                duration_ms,
                now,
            ],
        )?;
        Ok(())
    }

    /// 标记曲目缓存完整并记录字节数。`downloaded` 仅在真实下载成功时为 true；
    /// 启动对账和已有文件命中不得伪造下载时间。
    pub(super) fn mark_complete(
        &self,
        hash: &str,
        bytes: u64,
        downloaded: bool,
    ) -> Result<bool, MetadataStoreError> {
        let now_secs = now_epoch_secs();
        let now_ms = now_epoch_ms();
        let updated = self.conn.execute(
            "UPDATE cached_tracks
                SET complete = 1, bytes = ?2, updated_at = ?3,
                    downloaded_at_ms = CASE WHEN ?4 THEN ?5 ELSE downloaded_at_ms END
              WHERE hash = ?1",
            params![hash, bytes as i64, now_secs, downloaded, now_ms],
        )?;
        Ok(updated > 0)
    }

    /// 记录一次完整缓存命中。一次代理请求只调用一次；Range 请求也属于真实读取。
    pub(super) fn record_cache_hit(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks
                SET cache_hit_count = cache_hit_count + 1, updated_at = ?2
              WHERE hash = ?1",
            params![hash, now_epoch_secs()],
        )?;
        Ok(updated > 0)
    }

    /// 记录已实际进入 Playing 的播放会话。session/generation 唯一围栏确保重试、
    /// 重复快照或并发观察者不会重复计数。
    pub(super) fn record_play(
        &self,
        hash: &str,
        session: &str,
        generation: u64,
        requested: bool,
    ) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks SET
                play_count = play_count + 1,
                requested_play_count = requested_play_count + CASE WHEN ?6 THEN 1 ELSE 0 END,
                pool_play_count = pool_play_count + CASE WHEN ?6 THEN 0 ELSE 1 END,
                last_played_at_ms = ?4,
                last_play_generation = ?3,
                last_play_session = ?2,
                updated_at = ?5
              WHERE hash = ?1
                AND NOT (last_play_generation = ?3 AND last_play_session = ?2)",
            params![
                hash,
                session,
                generation as i64,
                now_epoch_ms(),
                now_epoch_secs(),
                requested
            ],
        )?;
        Ok(updated > 0)
    }

    /// 记录会话最终播放失败。只存稳定错误码，不存错误消息、URL 或凭据。
    /// 同一 session/generation 的重试中间失败不会调用本方法。
    pub(super) fn record_failure(
        &self,
        hash: &str,
        session: &str,
        generation: u64,
        code: &str,
    ) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks SET
                failure_count = failure_count + 1,
                last_failure_code = ?4,
                last_failure_generation = ?3,
                last_failure_session = ?2,
                updated_at = ?5
              WHERE hash = ?1
                AND NOT (last_failure_generation = ?3 AND last_failure_session = ?2)",
            params![
                hash,
                session,
                generation as i64,
                sanitize_failure_code(code),
                now_epoch_secs()
            ],
        )?;
        Ok(updated > 0)
    }

    /// 清除曲目完整标记与字节数（对账发现磁盘完整文件缺失时调用），
    /// 保留身份记录供后续重新下载。返回是否命中现有记录。
    pub(super) fn clear_complete(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks SET complete = 0, bytes = 0
              WHERE hash = ?1 AND (complete != 0 OR bytes != 0)",
            params![hash],
        )?;
        Ok(updated > 0)
    }

    /// 刷新曲目最近使用时间（updated_at）。返回是否命中现有记录。
    pub(super) fn touch(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks SET updated_at = ?2 WHERE hash = ?1",
            params![hash, now_epoch_secs()],
        )?;
        Ok(updated > 0)
    }

    /// 清零单曲统计，不删除缓存文件、曲目身份、元数据或歌词索引。
    pub(super) fn reset_statistics(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let updated = self.conn.execute(
            "UPDATE cached_tracks SET
                play_count = 0, requested_play_count = 0, pool_play_count = 0,
                cache_hit_count = 0, failure_count = 0,
                last_played_at_ms = NULL, last_failure_code = NULL,
                last_play_generation = 0, last_play_session = NULL,
                last_failure_generation = 0, last_failure_session = NULL
              WHERE hash = ?1",
            params![hash],
        )?;
        Ok(updated > 0)
    }

    /// 删除曲目记录；歌词经外键级联一并删除。返回是否实际删除。
    #[cfg(test)]
    pub(super) fn remove(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let deleted = self
            .conn
            .execute("DELETE FROM cached_tracks WHERE hash = ?1", params![hash])?;
        Ok(deleted > 0)
    }

    /// upsert 曲目的歌词正文（JSON 字节；bytes 取正文长度）。
    /// 曲目记录必须已存在（外键约束），否则返回错误。
    pub(super) fn upsert_lyrics(
        &self,
        hash: &str,
        lyrics_json: &[u8],
    ) -> Result<(), MetadataStoreError> {
        self.conn.execute(
            "INSERT INTO cached_lyrics (hash, lyrics_json, bytes, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO UPDATE SET
                lyrics_json = excluded.lyrics_json,
                bytes = excluded.bytes,
                updated_at = excluded.updated_at",
            params![
                hash,
                lyrics_json,
                lyrics_json.len() as i64,
                now_epoch_secs()
            ],
        )?;
        Ok(())
    }

    /// 登记对账发现的孤儿完整文件：磁盘上存在 `.complete` 文件但数据库
    /// 无对应记录时，按 hash 插入一条无身份记录（source/id 为 NULL，
    /// 不覆盖任何已有身份）；若记录已存在则只刷新完整状态与大小。
    /// 后续解析到真实身份后由 [`Self::upsert_track`] 补齐 source/id。
    pub(super) fn insert_unknown_complete(
        &self,
        hash: &str,
        bytes: u64,
    ) -> Result<(), MetadataStoreError> {
        let now = now_epoch_secs();
        self.conn.execute(
            "INSERT INTO cached_tracks
                (hash, source, id, title, artists, album, duration_ms,
                 complete, bytes, created_at, updated_at)
             VALUES (?1, NULL, NULL, NULL, '[]', NULL, NULL, 1, ?2, ?3, ?3)
             ON CONFLICT(hash) DO UPDATE SET
                complete = 1,
                bytes = excluded.bytes,
                updated_at = excluded.updated_at",
            params![hash, bytes as i64, now],
        )?;
        Ok(())
    }

    /// 删除曲目的歌词缓存。返回是否实际删除。
    pub(super) fn remove_lyrics(&self, hash: &str) -> Result<bool, MetadataStoreError> {
        let deleted = self
            .conn
            .execute("DELETE FROM cached_lyrics WHERE hash = ?1", params![hash])?;
        Ok(deleted > 0)
    }

    /// 查询单条曲目记录（仅供存储层测试核对写入结果）。
    #[cfg(test)]
    pub(super) fn track(&self, hash: &str) -> Result<Option<TrackRecord>, MetadataStoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS} FROM cached_tracks WHERE hash = ?1"
        ))?;
        let mut rows = stmt.query(params![hash])?;
        match rows.next()? {
            Some(row) => Ok(Some(track_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// 全量查询所有曲目记录（对账基准：与磁盘 `.complete` 文件逐条比对）。
    pub(super) fn all_tracks(&self) -> Result<Vec<TrackRecord>, MetadataStoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS} FROM cached_tracks ORDER BY updated_at DESC, hash ASC"
        ))?;
        let mut rows = stmt.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(track_from_row(row)?);
        }
        Ok(records)
    }

    /// 读取曲目的缓存歌词正文（未缓存返回 None）。
    pub(super) fn get_lyrics_json(
        &self,
        hash: &str,
    ) -> Result<Option<Vec<u8>>, MetadataStoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT lyrics_json FROM cached_lyrics WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 统计整体缓存情况，并给出指定曲目各自的记录状态。
    pub(super) fn stats(
        &self,
        keys: &[SongKey],
    ) -> Result<(MetadataStats, Vec<TrackStatus>), MetadataStoreError> {
        let (total, complete, bytes, plays, requested_plays, pool_plays, hits, failures): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = self.conn.query_row(
            "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN complete = 1 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN complete = 1 THEN bytes ELSE 0 END), 0),
                        COALESCE(SUM(play_count), 0),
                        COALESCE(SUM(requested_play_count), 0),
                        COALESCE(SUM(pool_play_count), 0),
                        COALESCE(SUM(cache_hit_count), 0),
                        COALESCE(SUM(failure_count), 0)
                   FROM cached_tracks",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let (lyrics_count, lyrics_bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM cached_lyrics",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut statuses = Vec::with_capacity(keys.len());
        for key in keys {
            let hash = cache_key_hash(key);
            type StatusRow = (
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                Option<i64>,
                Option<String>,
            );
            let row: Option<StatusRow> = self
                .conn
                .query_row(
                    "SELECT complete, bytes, play_count, requested_play_count, pool_play_count,
                            cache_hit_count, failure_count, last_played_at_ms, last_failure_code
                       FROM cached_tracks WHERE hash = ?1",
                    params![hash],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ))
                    },
                )
                .optional()?;
            statuses.push(match row {
                Some((
                    complete,
                    bytes,
                    plays,
                    requested_plays,
                    pool_plays,
                    hits,
                    failures,
                    last_played,
                    failure_code,
                )) => TrackStatus {
                    key: key.clone(),
                    recorded: true,
                    complete: complete != 0,
                    bytes: Some(bytes.max(0) as u64),
                    play_count: plays.max(0) as u64,
                    requested_play_count: requested_plays.max(0) as u64,
                    pool_play_count: pool_plays.max(0) as u64,
                    cache_hit_count: hits.max(0) as u64,
                    failure_count: failures.max(0) as u64,
                    last_played_at_ms: last_played.map(|value| value.max(0) as u64),
                    last_failure_code: failure_code,
                },
                None => TrackStatus {
                    key: key.clone(),
                    recorded: false,
                    complete: false,
                    bytes: None,
                    play_count: 0,
                    requested_play_count: 0,
                    pool_play_count: 0,
                    cache_hit_count: 0,
                    failure_count: 0,
                    last_played_at_ms: None,
                    last_failure_code: None,
                },
            });
        }
        Ok((
            MetadataStats {
                total_tracks: total.max(0) as usize,
                complete_tracks: complete.max(0) as usize,
                complete_bytes: bytes.max(0) as u64,
                lyrics_tracks: lyrics_count.max(0) as usize,
                lyrics_bytes: lyrics_bytes.max(0) as u64,
                play_count: plays.max(0) as u64,
                requested_play_count: requested_plays.max(0) as u64,
                pool_play_count: pool_plays.max(0) as u64,
                cache_hit_count: hits.max(0) as u64,
                failure_count: failures.max(0) as u64,
            },
            statuses,
        ))
    }

    /// 分页查询缓存歌曲列表（排序键映射到固定列，hash 作次序兜底），
    /// 返回 (总条数, 本页记录)。
    pub(super) fn list_tracks(
        &self,
        offset: usize,
        limit: usize,
        sort: super::CacheTrackSortKey,
        ascending: bool,
    ) -> Result<(usize, Vec<TrackRecord>), MetadataStoreError> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM cached_tracks WHERE complete = 1",
            [],
            |row| row.get(0),
        )?;
        let direction = if ascending { "ASC" } else { "DESC" };
        let column = sort.column();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS} FROM cached_tracks
              WHERE complete = 1
              ORDER BY {column} {direction}, hash ASC LIMIT ?2 OFFSET ?1"
        ))?;
        let offset = i64::try_from(offset).map_err(|_| MetadataStoreError::PaginationOverflow {
            parameter: "offset",
            value: offset,
        })?;
        let limit = i64::try_from(limit).map_err(|_| MetadataStoreError::PaginationOverflow {
            parameter: "limit",
            value: limit,
        })?;
        let mut rows = stmt.query(params![offset, limit])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(track_from_row(row)?);
        }
        Ok((total.max(0) as usize, records))
    }

    /// 打开连接后的一次性初始化：PRAGMA、建表、schema 版本。
    fn initialize(&self) -> Result<(), MetadataStoreError> {
        // WAL 模式必须在事务外设置；查询返回模式名，忽略。
        let _mode: String = self
            .conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        self.conn
            .pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS as i64)?;
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            // 全新数据库：一次性创建当前完整 schema。
            0 => {
                self.conn.execute_batch(SCHEMA_SQL)?;
                self.conn
                    .pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            // 当前版本：幂等确认全部表存在。
            SCHEMA_VERSION => {
                self.conn.execute_batch(SCHEMA_SQL)?;
            }
            other => {
                return Err(MetadataStoreError::UnsupportedSchema { version: other });
            }
        }
        Ok(())
    }
}

/// 把一行 cached_tracks 记录解析为 [`TrackRecord`]。
fn track_from_row(row: &rusqlite::Row<'_>) -> Result<TrackRecord, MetadataStoreError> {
    let title: Option<String> = row.get(3)?;
    let artists_json: String = row.get(4)?;
    let album: Option<String> = row.get(5)?;
    let duration_ms: Option<i64> = row.get(6)?;
    let complete: i64 = row.get(7)?;
    let bytes: i64 = row.get(8)?;
    let metadata = match title {
        Some(title) => Some(TrackMetadata {
            title,
            artists: serde_json::from_str(&artists_json)?,
            album,
            duration_ms: duration_ms.map(|ms| ms as u64),
        }),
        None => None,
    };
    Ok(TrackRecord {
        hash: row.get(0)?,
        source: row.get(1)?,
        id: row.get(2)?,
        metadata,
        complete: complete != 0,
        bytes: bytes.max(0) as u64,
        created_at_epoch_secs: row.get(9)?,
        updated_at_epoch_secs: row.get(10)?,
        play_count: row.get::<_, i64>(11)?.max(0) as u64,
        requested_play_count: row.get::<_, i64>(12)?.max(0) as u64,
        pool_play_count: row.get::<_, i64>(13)?.max(0) as u64,
        cache_hit_count: row.get::<_, i64>(14)?.max(0) as u64,
        failure_count: row.get::<_, i64>(15)?.max(0) as u64,
        last_played_at_ms: row
            .get::<_, Option<i64>>(16)?
            .map(|value| value.max(0) as u64),
        last_failure_code: row.get(17)?,
        downloaded_at_ms: row
            .get::<_, Option<i64>>(18)?
            .map(|value| value.max(0) as u64),
    })
}

/// 当前 Unix 时间（秒）。
fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// 失败码是低基数字段：限制字符集和长度，防止调用方误把错误消息或敏感值入库。
fn sanitize_failure_code(code: &str) -> String {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        code.to_owned()
    } else {
        "playback_failed".to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum MetadataStoreError {
    #[error("创建元数据目录失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite 操作失败: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("序列化歌词失败: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("分页参数 {parameter} 超出 SQLite i64 范围: {value}")]
    PaginationOverflow {
        parameter: &'static str,
        value: usize,
    },
    #[error("不支持的数据库 schema 版本: {version}（当前版本 v1，不提供迁移）")]
    UnsupportedSchema { version: i64 },
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_directory() -> PathBuf {
        std::env::temp_dir().join(format!("miliastra-metadata-{}", uuid::Uuid::new_v4()))
    }

    fn key(source: &str, id: &str) -> SongKey {
        SongKey::new(source, id).unwrap()
    }

    fn sample_metadata() -> TrackMetadata {
        TrackMetadata {
            title: "测试歌曲".to_owned(),
            artists: vec!["歌手甲".to_owned(), "歌手乙".to_owned()],
            album: Some("示例专辑".to_owned()),
            duration_ms: Some(210_000),
        }
    }

    #[test]
    fn open_initializes_wal_mode_and_schema_version() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();

        assert!(directory.join(DATABASE_FILE_NAME).is_file());
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let foreign_keys: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let busy_timeout: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS as i64);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn cache_schema_can_join_an_existing_request_state_database() {
        let directory = temp_directory();
        std::fs::create_dir_all(&directory).unwrap();
        let database_path = directory.join(DATABASE_FILE_NAME);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE request_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    schema_version INTEGER NOT NULL,
                    snapshot TEXT NOT NULL
                )",
            )
            .unwrap();
        drop(connection);

        let store =
            MetadataStore::open(&directory).expect("缓存表应能加入已包含请求状态表的统一数据库");
        let request_state_exists: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'request_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cached_tracks_exists: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cached_tracks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(request_state_exists, 1);
        assert_eq!(cached_tracks_exists, 1);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn schema_never_stores_urls_headers_or_credentials() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();

        for table in ["cached_tracks", "cached_lyrics"] {
            let mut stmt = store
                .conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let columns: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .map(|column| column.unwrap())
                .collect();
            for column in &columns {
                let lower = column.to_ascii_lowercase();
                for forbidden in [
                    "url",
                    "header",
                    "credential",
                    "token",
                    "cookie",
                    "password",
                    "account",
                    "secret",
                ] {
                    assert!(
                        !lower.contains(forbidden),
                        "表 {table} 的列 {column} 疑似存储敏感信息"
                    );
                }
            }
        }

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn upsert_track_inserts_then_updates_in_place() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "song-1");
        let hash = cache_key_hash(&track_key);

        store
            .upsert_track(&track_key, Some(&sample_metadata()))
            .unwrap();
        let first = store.track(&hash).unwrap().unwrap();
        assert_eq!(first.source.as_deref(), Some("kugou"));
        assert_eq!(first.id.as_deref(), Some("song-1"));
        assert_eq!(first.metadata, Some(sample_metadata()));

        // 再次 upsert：不新增行，元数据被覆盖，created_at 不变。
        let updated_metadata = TrackMetadata {
            title: "新标题".to_owned(),
            ..sample_metadata()
        };
        store
            .upsert_track(&track_key, Some(&updated_metadata))
            .unwrap();
        let second = store.track(&hash).unwrap().unwrap();
        assert_eq!(second.metadata.unwrap().title, "新标题");
        assert_eq!(second.created_at_epoch_secs, first.created_at_epoch_secs);

        let total: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM cached_tracks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn upsert_without_metadata_keeps_existing_metadata() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "song-2");
        let hash = cache_key_hash(&track_key);

        store
            .upsert_track(&track_key, Some(&sample_metadata()))
            .unwrap();
        store.upsert_track(&track_key, None).unwrap();

        let record = store.track(&hash).unwrap().unwrap();
        assert_eq!(record.metadata, Some(sample_metadata()));
        assert_eq!(record.source.as_deref(), Some("kugou"));

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn complete_clear_touch_and_remove_lifecycle() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "song-3");
        let hash = cache_key_hash(&track_key);

        // 未记录时各更新原语返回 false。
        assert!(!store.mark_complete(&hash, 1024, false).unwrap());
        assert!(!store.touch(&hash).unwrap());
        assert!(!store.remove(&hash).unwrap());

        store.upsert_track(&track_key, None).unwrap();
        assert!(store.mark_complete(&hash, 4096, true).unwrap());
        let record = store.track(&hash).unwrap().unwrap();
        assert!(record.complete);
        assert_eq!(record.bytes, 4096);

        assert!(store.clear_complete(&hash).unwrap());
        let record = store.track(&hash).unwrap().unwrap();
        assert!(!record.complete);
        assert_eq!(record.bytes, 0);

        assert!(store.touch(&hash).unwrap());
        assert!(store.remove(&hash).unwrap());
        assert!(store.track(&hash).unwrap().is_none());
        // 再次删除返回 false。
        assert!(!store.remove(&hash).unwrap());

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn lyrics_upsert_remove_and_cascade_with_track() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "song-4");
        let hash = cache_key_hash(&track_key);

        // 曲目记录不存在时，歌词正文写入违反外键约束。
        assert!(store.upsert_lyrics(&hash, b"{\"lines\":[]}").is_err());

        store.upsert_track(&track_key, None).unwrap();
        store.upsert_lyrics(&hash, b"{\"lines\":[]}").unwrap();
        assert_eq!(
            store.get_lyrics_json(&hash).unwrap().as_deref(),
            Some(b"{\"lines\":[]}".as_slice())
        );

        // 重复 upsert 幂等：不新增行，正文与大小被覆盖。
        let second = b"{\"lines\":[\"a\",\"b\"]}";
        store.upsert_lyrics(&hash, second).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM cached_lyrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let (bytes, json): (i64, Vec<u8>) = store
            .conn
            .query_row(
                "SELECT bytes, lyrics_json FROM cached_lyrics WHERE hash = ?1",
                params![hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(bytes, second.len() as i64);
        assert_eq!(json, second);

        assert!(store.remove_lyrics(&hash).unwrap());
        assert!(!store.remove_lyrics(&hash).unwrap());
        assert!(store.get_lyrics_json(&hash).unwrap().is_none());

        // 删除曲目记录时歌词正文级联删除。
        store.upsert_lyrics(&hash, b"lyrics").unwrap();
        assert!(store.remove(&hash).unwrap());
        assert!(store.get_lyrics_json(&hash).unwrap().is_none());

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn insert_unknown_complete_then_fill_identity() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "song-5");
        let hash = cache_key_hash(&track_key);

        // 模拟对账发现的孤儿完整文件：先登记无身份记录。
        store.insert_unknown_complete(&hash, 65_536).unwrap();
        let record = store.track(&hash).unwrap().unwrap();
        assert!(record.source.is_none() && record.id.is_none());
        assert!(record.complete);
        assert_eq!(record.bytes, 65_536);

        // 重复登记幂等：只刷新状态，不产生第二行。
        store.insert_unknown_complete(&hash, 70_000).unwrap();
        let total: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM cached_tracks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(store.track(&hash).unwrap().unwrap().bytes, 70_000);

        // 后续解析到真实身份：upsert 补齐 source/id，完整状态保留。
        store
            .upsert_track(&track_key, Some(&sample_metadata()))
            .unwrap();
        let record = store.track(&hash).unwrap().unwrap();
        assert_eq!(record.source.as_deref(), Some("kugou"));
        assert_eq!(record.id.as_deref(), Some("song-5"));
        assert_eq!(record.metadata, Some(sample_metadata()));
        assert!(record.complete);
        assert_eq!(record.bytes, 70_000);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn stats_reports_totals_and_per_track_statuses() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let complete_key = key("kugou", "done");
        let partial_key = key("kugou", "partial");
        let unknown_key = key("kugou", "missing");

        store
            .upsert_track(&complete_key, Some(&sample_metadata()))
            .unwrap();
        store
            .mark_complete(&cache_key_hash(&complete_key), 8192, true)
            .unwrap();
        store.upsert_track(&partial_key, None).unwrap();
        let lyrics_json = b"512-bytes-lyrics";
        store
            .upsert_lyrics(&cache_key_hash(&complete_key), lyrics_json)
            .unwrap();

        let (stats, statuses) = store
            .stats(&[complete_key, partial_key, unknown_key])
            .unwrap();
        assert_eq!(stats.total_tracks, 2);
        assert_eq!(stats.complete_tracks, 1);
        assert_eq!(stats.complete_bytes, 8192);
        assert_eq!(stats.lyrics_tracks, 1);
        assert_eq!(stats.lyrics_bytes, lyrics_json.len() as u64);

        assert_eq!(statuses.len(), 3);
        assert!(statuses[0].recorded && statuses[0].complete);
        assert_eq!(statuses[0].bytes, Some(8192));
        assert!(statuses[1].recorded && !statuses[1].complete);
        assert!(!statuses[2].recorded);
        assert_eq!(statuses[2].bytes, None);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn list_tracks_paginates_by_recent_use() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        for id in ["a", "b", "c"] {
            let track_key = key("kugou", id);
            store.upsert_track(&track_key, None).unwrap();
            store
                .mark_complete(&cache_key_hash(&track_key), 1024, true)
                .unwrap();
        }
        // 未完整历史元数据不得出现在磁盘缓存列表或 total 中。
        store.upsert_track(&key("kugou", "idle"), None).unwrap();
        // 手动铺时间：c 最新、a 最旧，验证按最近使用倒序。
        store
            .conn
            .execute(
                "UPDATE cached_tracks SET updated_at = ?1 WHERE id = ?2",
                params![1000, "a"],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE cached_tracks SET updated_at = ?1 WHERE id = ?2",
                params![2000, "b"],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE cached_tracks SET updated_at = ?1 WHERE id = ?2",
                params![3000, "c"],
            )
            .unwrap();

        let (total, page1) = store
            .list_tracks(0, 2, crate::cache::CacheTrackSortKey::LastUsed, false)
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].id.as_deref(), Some("c"));
        assert_eq!(page1[1].id.as_deref(), Some("b"));

        let (total, page2) = store
            .list_tracks(2, 2, crate::cache::CacheTrackSortKey::LastUsed, false)
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].id.as_deref(), Some("a"));

        if usize::BITS > i64::BITS {
            let error = store
                .list_tracks(
                    usize::MAX,
                    1,
                    crate::cache::CacheTrackSortKey::LastUsed,
                    false,
                )
                .unwrap_err();
            assert!(matches!(
                error,
                MetadataStoreError::PaginationOverflow {
                    parameter: "offset",
                    ..
                }
            ));
        }

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn list_tracks_sorts_by_requested_play_count() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        for (id, count) in [("low", 1u64), ("high", 9), ("mid", 5)] {
            let track_key = key("kugou", id);
            store.upsert_track(&track_key, None).unwrap();
            store
                .mark_complete(&cache_key_hash(&track_key), 1024, true)
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE cached_tracks SET requested_play_count = ?1 WHERE id = ?2",
                    params![count, id],
                )
                .unwrap();
        }

        let (_, descending) = store
            .list_tracks(
                0,
                10,
                crate::cache::CacheTrackSortKey::RequestedPlayCount,
                false,
            )
            .unwrap();
        let ids: Vec<&str> = descending
            .iter()
            .map(|record| record.id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, ["high", "mid", "low"]);

        let (_, ascending) = store
            .list_tracks(
                0,
                10,
                crate::cache::CacheTrackSortKey::RequestedPlayCount,
                true,
            )
            .unwrap();
        let ids: Vec<&str> = ascending
            .iter()
            .map(|record| record.id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, ["low", "mid", "high"]);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn events_are_fenced_and_aggregated_without_sensitive_failure_text() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "statistics");
        let hash = cache_key_hash(&track_key);
        store
            .upsert_track(&track_key, Some(&sample_metadata()))
            .unwrap();

        assert!(store.record_play(&hash, "session-a", 7, true).unwrap());
        assert!(!store.record_play(&hash, "session-a", 7, true).unwrap());
        assert!(store.record_play(&hash, "session-b", 8, false).unwrap());
        assert!(store.record_cache_hit(&hash).unwrap());
        assert!(store.record_cache_hit(&hash).unwrap());
        assert!(
            store
                .record_failure(&hash, "session-b", 8, "decode_failure?token=secret")
                .unwrap()
        );
        assert!(
            !store
                .record_failure(&hash, "session-b", 8, "another_failure")
                .unwrap()
        );
        store.mark_complete(&hash, 4096, true).unwrap();

        let record = store.track(&hash).unwrap().unwrap();
        assert_eq!(record.play_count, 2);
        assert_eq!(record.requested_play_count, 1);
        assert_eq!(record.pool_play_count, 1);
        assert_eq!(record.cache_hit_count, 2);
        assert_eq!(record.failure_count, 1);
        assert_eq!(record.last_failure_code.as_deref(), Some("playback_failed"));
        assert!(record.last_played_at_ms.is_some());
        assert!(record.downloaded_at_ms.is_some());
        let (stats, _) = store.stats(&[]).unwrap();
        assert_eq!(
            (stats.play_count, stats.cache_hit_count, stats.failure_count),
            (2, 2, 1)
        );

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn concurrent_connections_do_not_lose_atomic_cache_hit_counts() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "concurrent");
        let hash = cache_key_hash(&track_key);
        store.upsert_track(&track_key, None).unwrap();
        drop(store);

        let mut workers = Vec::new();
        for _ in 0..4 {
            let directory = directory.clone();
            let hash = hash.clone();
            workers.push(std::thread::spawn(move || {
                let store = MetadataStore::open(&directory).unwrap();
                for _ in 0..25 {
                    assert!(store.record_cache_hit(&hash).unwrap());
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let store = MetadataStore::open(&directory).unwrap();
        assert_eq!(store.track(&hash).unwrap().unwrap().cache_hit_count, 100);
        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn reset_statistics_preserves_cache_metadata_and_lyrics() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        let track_key = key("kugou", "reset");
        let hash = cache_key_hash(&track_key);
        store
            .upsert_track(&track_key, Some(&sample_metadata()))
            .unwrap();
        store.mark_complete(&hash, 4096, true).unwrap();
        store.upsert_lyrics(&hash, b"lyrics-body").unwrap();
        store.record_play(&hash, "session-reset", 9, true).unwrap();
        store.record_cache_hit(&hash).unwrap();
        store
            .record_failure(&hash, "session-reset", 9, "decode_failure")
            .unwrap();

        assert!(store.reset_statistics(&hash).unwrap());
        let record = store.track(&hash).unwrap().unwrap();
        assert!(record.complete);
        assert_eq!(record.bytes, 4096);
        assert_eq!(record.id.as_deref(), Some("reset"));
        assert!(record.metadata.is_some());
        assert!(record.downloaded_at_ms.is_some());
        assert_eq!(record.play_count, 0);
        assert_eq!(record.requested_play_count, 0);
        assert_eq!(record.pool_play_count, 0);
        assert_eq!(record.cache_hit_count, 0);
        assert_eq!(record.failure_count, 0);
        assert!(record.last_played_at_ms.is_none());
        assert!(record.last_failure_code.is_none());
        assert_eq!(store.stats(&[]).unwrap().0.lyrics_tracks, 1);

        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn open_rejects_unsupported_schema_version() {
        let directory = temp_directory();
        let store = MetadataStore::open(&directory).unwrap();
        store.conn.pragma_update(None, "user_version", 99).unwrap();
        drop(store);

        let error = MetadataStore::open(&directory).unwrap_err();
        assert!(matches!(
            error,
            MetadataStoreError::UnsupportedSchema { version: 99 }
        ));

        std::fs::remove_dir_all(&directory).unwrap();
    }
}
