//! 轻量记忆存储：问答原文、事实、中期摘要与大厅共享记忆。
//!
//! 三层保留（近期原文 → 中期摘要 → 远期事实），只用 SQLite 单库单表。
//! 容量靠压缩与硬上限兜底，读取侧只做异常保护。

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::budget::{
    ACTOR_FACTS_BUDGET, ACTOR_READ_MIN_TURNS, ACTOR_READ_TOKEN_BUDGET, COMPRESS_TARGET_PERCENT,
    COMPRESS_TRIGGER_PERCENT, HALL_BOT_READ_MAX_LINES, HALL_BOT_READ_TOKEN_BUDGET,
    HALL_READ_BUDGET, HARD_CAP_PERCENT, MAX_FACT_CHARS, MAX_FACTS_PER_ACTOR,
};
use super::config::HallSpeakerRole;

/// 取模型返回的摘要正文。模型可能套了 JSON 或引号，这里只做最小清洗。
pub(crate) fn parse_summary(reply: &str) -> String {
    let text = reply.trim();
    // 兼容 {"summary": "…"} 这种结构化返回。
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(summary) = value.get("summary").and_then(|v| v.as_str())
    {
        return summary.trim().to_string();
    }
    text.trim_matches(['"', '“', '”']).trim().to_string()
}

/// 把一批原文渲染成压缩输入。
fn render_records(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一轮问答。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredTurn {
    /// 提问人。历史里每条都带名字，模型才能分清「谁问的」。
    pub(crate) asker: String,
    pub(crate) question: String,
    pub(crate) answer: String,
}

/// 提问人前缀的统一格式。
///
/// 大厅里同时在场的玩家很多，模型必须能在历史里分清谁是谁；
/// 个人记忆虽然只有一个人，也用同一格式，保证两层的提示词风格一致。
pub(crate) fn asker_prefix(asker: &str) -> String {
    let asker = asker.trim();
    if asker.is_empty() {
        "[未知]".to_string()
    } else {
        format!("[{asker}]")
    }
}

/// 组装提示词所需的记忆快照。
///
/// 分三层，靠预算各自维护：
/// - `facts`：远期，AI 抽取的稳定信息，永不压缩
/// - `summary`：中期，溢出的原文滚动压缩成一段话
/// - `turns`：近期，问答原文
#[derive(Clone, Debug, Default)]
pub(crate) struct ChatContext {
    pub(crate) facts: Vec<String>,
    pub(crate) summary: Option<String>,
    pub(crate) turns: Vec<StoredTurn>,
}

/// 大厅共享记忆快照：所有人最近说过的话 + 滚动压缩的大厅摘要。
#[derive(Clone, Debug, Default)]
pub(crate) struct HallContext {
    pub(crate) summary: Option<String>,
    /// (说话人, 内容, 是否 BOT 自己)，按时间从旧到新。
    pub(crate) lines: Vec<(String, String, HallSpeakerRole)>,
}

/// 历史读取起点：append-only，只做异常保护，不参与常规裁剪。
///
/// `tokens` 是从旧到新的每轮 token。返回需要跳过的最旧轮数。
///
/// 规则：从最新往旧累计，累计超过 [ACTOR_READ_TOKEN_BUDGET] 就停，**整段保留**
/// 最近的连续历史（不跳行、不挑行）；同时至少保留最近
/// [ACTOR_READ_MIN_TURNS] 轮，避免单条异常大的消息把历史读成空。
///
/// 正常路径返回 0：压缩（82% 触发、降到 15%）已经把落库量压在预算以下，
/// 读取全部原文 = `messages` 前缀逐字节稳定 = provider 前缀缓存不失效。
pub(crate) fn actor_turn_read_start(tokens: &[usize]) -> usize {
    let total = tokens.len();
    let mut used = 0usize;
    let mut keep = 0usize;
    for tokens in tokens.iter().rev() {
        if keep >= ACTOR_READ_MIN_TURNS && used.saturating_add(*tokens) > ACTOR_READ_TOKEN_BUDGET {
            break;
        }
        used += *tokens;
        keep += 1;
    }
    total - keep
}

/// 轻量记忆存储。
/// 面板用的玩家记忆规模摘要。
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ActorMemorySummary {
    pub(crate) actor: String,
    pub(crate) facts: usize,
    pub(crate) turns: usize,
    pub(crate) tokens: usize,
}
pub(crate) struct AiChatMemory {
    /// 单连接 + 一把锁：库操作用 `&self`，不需要每轮重新打开。
    ///
    /// `pub(super)` 而不是私有：迁移/遗忘级联用例要直接校验表结构与跨表行数，
    /// 那比给每个断言再造一个查询 API 更清楚。放到 `ai_chat` 这一层即可，
    /// 不需要整个 crate 都看见（拆分前它在本模块内是私有的）。
    pub(super) connection: Mutex<Connection>,
}

/// 一次压缩需要做什么：取出哪些原文、旧摘要是什么。
///
/// 只在真正超预算时构造，避免每次问答都去统计。
pub(crate) struct CompressionPlan {
    pub(crate) scope: String,
    pub(crate) previous_summary: String,
    pub(crate) records: Vec<String>,
    /// 这些原文对应的行 id 与所在表：压缩成功后才按它们删除。
    pub(crate) source_table: &'static str,
    pub(crate) source_ids: Vec<i64>,
}

impl AiChatMemory {
    pub(crate) fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub(crate) fn from_connection(connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(3))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS ai_chat_turns (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 actor    TEXT    NOT NULL,
                 question TEXT    NOT NULL,
                 answer   TEXT    NOT NULL,
                 tokens   INTEGER NOT NULL,
                 added_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ai_chat_turns_actor ON ai_chat_turns(actor, id);
             CREATE TABLE IF NOT EXISTS ai_chat_facts (
                 actor    TEXT    NOT NULL,
                 fact     TEXT    NOT NULL,
                 tokens   INTEGER NOT NULL,
                 added_at INTEGER NOT NULL,
                 PRIMARY KEY (actor, fact)
             );
             CREATE INDEX IF NOT EXISTS idx_ai_chat_facts_actor ON ai_chat_facts(actor, added_at);
             CREATE TABLE IF NOT EXISTS ai_chat_bot_lines (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 text     TEXT    NOT NULL,
                 tokens   INTEGER NOT NULL,
                 added_at INTEGER NOT NULL
             );
             -- 中期摘要：把溢出的原文压成一段话，滚动覆盖。
             -- scope 用 'actor:<名字>' 或 'hall' 区分个人记忆与大厅共享记忆。
             CREATE TABLE IF NOT EXISTS ai_chat_summaries (
                 scope    TEXT    PRIMARY KEY,
                 summary  TEXT    NOT NULL,
                 tokens   INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             -- 大厅原文：记录**所有人在大厅说的话**，全局共享。
             -- role 区分玩家发言与 BOT 自己的发言：BOT 的旧话会同时进
             -- bot_lines 被再读一遍，模型容易把它当成玩家说过的话。
             -- 只加标记、不删内容，符合「大厅保留完整对话」。
             CREATE TABLE IF NOT EXISTS ai_chat_hall_turns (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 speaker  TEXT    NOT NULL,
                 text     TEXT    NOT NULL,
                 tokens   INTEGER NOT NULL,
                 added_at INTEGER NOT NULL,
                 role     TEXT    NOT NULL DEFAULT 'player',
                 -- 受众 key：玩家行=提问者，BOT 行=回复对象；空串为旧库遗留。
                 actor_key TEXT   NOT NULL DEFAULT ''
             );
             -- 改名历史：actor 是按昵称存的，玩家改名后旧事实会变成孤儿行。
             -- 这张表让「这个人用过哪些名字」成为结构事实，而不是靠模型从
             -- 大厅散文里猜——线上出现过三个人的曾用名被算成一个人。
             CREATE TABLE IF NOT EXISTS ai_chat_actor_names (
                 actor  TEXT    NOT NULL,
                 name   TEXT    NOT NULL,
                 state  TEXT    NOT NULL DEFAULT 'current',
                 since  INTEGER NOT NULL,
                 PRIMARY KEY (actor, name)
             );",
        )?;
        // 老库补列：CREATE TABLE IF NOT EXISTS 不会给已存在的表加列。
        // 用 PRAGMA 判断而不是 prepare，空表时 prepare 也会成功。
        let columns = connection
            .prepare("PRAGMA table_info(ai_chat_hall_turns)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !columns.iter().any(|name| name == "role") {
            connection.execute(
                "ALTER TABLE ai_chat_hall_turns ADD COLUMN role TEXT NOT NULL DEFAULT 'player'",
                [],
            )?;
        }
        // 方案 A：大厅按受众拆分。旧行 actor_key 空串，hall_context 仍返回，
        // 避免升级后旧数据凭空消失；新行才带上提问者/回复对象 key。
        if !columns.iter().any(|name| name == "actor_key") {
            connection.execute(
                "ALTER TABLE ai_chat_hall_turns ADD COLUMN actor_key TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("AI 聊天记忆存储不可用"))
    }

    /// 单个 scope 的记忆 scope 键。
    fn actor_scope(actor: &str) -> String {
        format!("actor:{actor}")
    }

    /// 大厅共享记忆的 scope 键。
    pub(crate) fn hall_scope() -> &'static str {
        "hall"
    }

    /// 读取某人的分层记忆：事实（远期）+ 摘要（中期）+ 原文（近期）。
    ///
    /// 原文是 **append-only** 读取：不按轮数、也不按固定块滑窗，读回全部
    /// 尚未被压缩的原文；只有总 token 超出 [ACTOR_READ_TOKEN_BUDGET] 时才
    /// 整段保留最近的连续历史（见 [actor_turn_read_start]）。
    ///
    /// 预算维护靠压缩与 `prune_actor`，读取侧只做异常保护：压缩把落库量
    /// 长期压在 82% 以下，所以正常情况下读取结果就是全表、逐轮只追加一条。
    pub(crate) fn context(&self, actor: &str) -> Result<ChatContext> {
        let connection = self.connection()?;
        let facts = connection
            .prepare(
                "SELECT fact FROM ai_chat_facts WHERE actor = ?1 ORDER BY added_at ASC, rowid ASC",
            )?
            .query_map([actor], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // 读取全部未压缩原文，token 用量一起取出来，由读取预算决定是否裁剪。
        // 不再 OFFSET 窗口：窗口起点每轮移动会让 `messages` 第一条历史变化，
        // 前缀缓存整段失效（见 `ACTOR_READ_TOKEN_BUDGET` 的说明）。
        let rows = connection
            .prepare(
                "SELECT question, answer, tokens FROM ai_chat_turns WHERE actor = ?1
                 ORDER BY id ASC",
            )?
            .query_map([actor], |row| {
                Ok((
                    StoredTurn {
                        asker: actor.to_string(),
                        question: row.get(0)?,
                        answer: row.get(1)?,
                    },
                    row.get::<_, i64>(2)?.max(0) as usize,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tokens: Vec<usize> = rows.iter().map(|(_, tokens)| *tokens).collect();
        let start = actor_turn_read_start(&tokens);
        let turns = rows
            .into_iter()
            .skip(start)
            .map(|(turn, _)| turn)
            .collect::<Vec<_>>();
        let summary = connection
            .query_row(
                "SELECT summary FROM ai_chat_summaries WHERE scope = ?1",
                [Self::actor_scope(actor)],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|text: &String| !text.trim().is_empty());
        Ok(ChatContext {
            facts,
            summary,
            turns,
        })
    }

    /// 读取大厅共享记忆：所有人在大厅说过的话 + 大厅摘要。
    ///
    /// 只读**最近**若干条：原来这条 SQL 没有 LIMIT，会全表读取——大厅每次
    /// 问答写 2 行，几百轮后单次请求就要塞进上万 token，而目标输出只有 160 字符。
    /// 落库预算（32000 是**保留**预算）与读取窗口是两回事，和多人对局的其它窗口同理。
    ///
    /// 方案 A：按受众过滤，当前 actor 的提问/BOT 对 TA 的回复不进 hall（个人
    /// messages 已有）；`actor_key` 传空串表示不过滤（面板全量视图）。
    pub(crate) fn hall_context(&self, actor_key: &str) -> Result<HallContext> {
        let actor_key = actor_key.trim();
        let connection = self.connection()?;
        // 从最新往回取：玩家行按 HALL_READ_BUDGET，Bot 行按条数/ token
        // 双预算单独截断，再反转成旧→新。
        //
        // 受众过滤：当前玩家的提问与 BOT 对他的回复只走个人 messages，不再
        // 在 hall 重复；其他人的发言/对别人的回复仍共享。`actor_key=''` 是
        // 升级前的旧行，没有归属信息，继续返回（否则升级即丢历史）；参数传
        // 空串表示面板要全量视图。
        let mut lines = Vec::new();
        let mut used_player = 0usize;
        let mut player_lines = 0usize;
        let mut bot_used = 0usize;
        let mut bot_lines = 0usize;
        {
            let mut stmt = connection.prepare(
                "SELECT speaker, text, role, tokens FROM ai_chat_hall_turns
                 WHERE actor_key <> ?1 OR actor_key = '' ORDER BY id DESC",
            )?;
            let mut rows = stmt.query([actor_key])?;
            while let Some(row) = rows.next()? {
                let tokens: usize = row.get::<_, i64>(3)?.max(0) as usize;
                let speaker: String = row.get(0)?;
                let text: String = row.get(1)?;
                let role: String = row.get(2)?;
                let role = HallSpeakerRole::from_str(&role);
                let keep = match role {
                    // 玩家行仍走整个 HALL_READ_BUDGET；至少保留最新一条，
                    // 否则极小预算会把大厅上下文清空。
                    HallSpeakerRole::Player => {
                        if player_lines == 0 || used_player + tokens <= HALL_READ_BUDGET {
                            player_lines += 1;
                            used_player += tokens;
                            true
                        } else {
                            false
                        }
                    }
                    // Bot 行独立限流：条数与 token 取先到者，避免 130 条自述
                    // 把大厅读窗口占满、或每轮多付数千 token。
                    HallSpeakerRole::Bot => {
                        if bot_lines < HALL_BOT_READ_MAX_LINES
                            && bot_used + tokens <= HALL_BOT_READ_TOKEN_BUDGET
                        {
                            bot_lines += 1;
                            bot_used += tokens;
                            true
                        } else {
                            false
                        }
                    }
                };
                if keep {
                    lines.push((speaker, text, role));
                }
            }
        }
        lines.reverse();
        let summary = connection
            .query_row(
                "SELECT summary FROM ai_chat_summaries WHERE scope = ?1",
                [Self::hall_scope()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|text: &String| !text.trim().is_empty());
        Ok(HallContext { summary, lines })
    }
    /// 记录一轮问答，并按单人 token 预算淘汰最旧记录。
    pub(crate) fn record_turn(
        &self,
        actor: &str,
        question: &str,
        answer: &str,
        budget: usize,
        now_ms: i64,
    ) -> Result<()> {
        let tokens = estimate_tokens(question) + estimate_tokens(answer);
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO ai_chat_turns(actor, question, answer, tokens, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![actor, question, answer, tokens as i64, now_ms],
        )?;
        prune_actor(&connection, actor, budget)?;
        Ok(())
    }

    /// 读取某个玩家的全部事实（旧→新）。
    ///
    /// 抽取时要把它交给模型，模型才知道哪些信息已经存在、哪些需要推翻；
    /// 不传的话它只能盲目追加，改名之后新旧称呼并存。
    pub(crate) fn facts(&self, actor: &str) -> Result<Vec<String>> {
        let connection = self.connection()?;
        let facts = connection
            .prepare(
                "SELECT fact FROM ai_chat_facts WHERE actor = ?1 ORDER BY added_at ASC, rowid ASC",
            )?
            .query_map([actor], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(facts)
    }

    /// 记录玩家的曾用名，用于跨改名的记忆归属。
    ///
    /// `actor` 是按昵称存的：玩家改名后新事实落在新行，旧行变成孤儿。
    /// 这张表把「这个人用过哪些名字」变成结构事实，
    /// 而不是靠模型从大厅散文里猜——线上出现过三个人的曾用名被算成一个人。
    pub(crate) fn record_actor_name(&self, actor: &str, name: &str, now_ms: i64) -> Result<()> {
        let name = name.trim();
        if actor.trim().is_empty() || name.is_empty() || actor == name {
            return Ok(());
        }
        let connection = self.connection()?;
        // 先把旧的 current 降级，再写入新的 current。
        connection.execute(
            "UPDATE ai_chat_actor_names SET state = 'former' WHERE actor = ?1 AND state = 'current'",
            [actor],
        )?;
        connection.execute(
            "INSERT INTO ai_chat_actor_names(actor, name, state, since) VALUES (?1, ?2, 'current', ?3) \
             ON CONFLICT(actor, name) DO UPDATE SET state = 'current', since = excluded.since",
            params![actor, name, now_ms],
        )?;
        Ok(())
    }

    /// 该玩家用过的所有名字（含当前）。
    pub(crate) fn actor_names(&self, actor: &str) -> Result<Vec<(String, bool)>> {
        let connection = self.connection()?;
        let rows = connection
            .prepare(
                "SELECT name, state FROM ai_chat_actor_names WHERE actor = ?1 ORDER BY since ASC",
            )?
            .query_map([actor], |row| {
                let name: String = row.get(0)?;
                let state: String = row.get(1)?;
                Ok((name, state == "current"))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 该玩家全部问答原文（旧→新），供重跑清洗使用。
    pub(crate) fn all_turns(&self, actor: &str) -> Result<Vec<(String, String)>> {
        let connection = self.connection()?;
        let turns = connection
            .prepare("SELECT question, answer FROM ai_chat_turns WHERE actor = ?1 ORDER BY id ASC")?
            .query_map([actor], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(turns)
    }

    /// 用一批新事实整体替换该玩家的事实表（重跑清洗专用）。
    ///
    /// 必须是「整体替换」而不是逐条增删：清洗的意义就是丢掉旧结论，
    /// 在旧结论上打补丁只会把错误保留下来。
    pub(crate) fn replace_facts(&self, actor: &str, facts: &[String], now_ms: i64) -> Result<()> {
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute("DELETE FROM ai_chat_facts WHERE actor = ?1", [actor])?;
        for fact in facts {
            let fact = fact.trim();
            if fact.is_empty() || fact.chars().count() > MAX_FACT_CHARS {
                continue;
            }
            transaction.execute(
                "INSERT INTO ai_chat_facts(actor, fact, tokens, added_at) VALUES (?1, ?2, ?3, ?4)",
                params![actor, fact, estimate_tokens(fact) as i64, now_ms],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 写入事实；重复只刷新时间。整条内容超过上限时丢弃而不截断。
    ///
    /// 事实走**独立预算**（[ACTOR_FACTS_BUDGET] / [MAX_FACTS_PER_ACTOR]）：
    /// 不再和原文共用同一个 token 池，避免超预算时把最该长期保留的事实先删掉。
    pub(crate) fn add_fact(&self, actor: &str, fact: &str, now_ms: i64) -> Result<()> {
        let fact = fact.trim();
        if fact.is_empty() || fact.chars().count() > MAX_FACT_CHARS {
            return Ok(());
        }
        let tokens = estimate_tokens(fact);
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO ai_chat_facts(actor, fact, tokens, added_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(actor, fact) DO UPDATE SET added_at = excluded.added_at",
            params![actor, fact, tokens as i64, now_ms],
        )?;
        prune_facts(&connection, actor)?;
        Ok(())
    }

    /// **legacy**：机器人历史已并入 `ai_chat_hall_turns`（role=bot），本表与
    /// 写入函数只为兼容旧数据保留，运行时不再调用；不要再用它作为提示词通道。
    #[allow(dead_code)]
    pub(crate) fn record_bot_line(&self, text: &str, budget: usize, now_ms: i64) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let tokens = estimate_tokens(text);
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO ai_chat_bot_lines(text, tokens, added_at) VALUES (?1, ?2, ?3)",
            params![text, tokens as i64, now_ms],
        )?;
        let total: i64 = connection.query_row(
            "SELECT COALESCE(SUM(tokens), 0) FROM ai_chat_bot_lines",
            [],
            |row| row.get(0),
        )?;
        let mut remaining = total;
        while remaining > budget as i64 {
            let Some((id, tokens)) = connection
                .query_row(
                    "SELECT id, tokens FROM ai_chat_bot_lines ORDER BY id ASC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
            else {
                break;
            };
            connection.execute("DELETE FROM ai_chat_bot_lines WHERE id = ?1", [id])?;
            remaining -= tokens;
        }
        Ok(())
    }

    /// 清空某人的记忆；返回是否确实删掉了内容。
    pub(crate) fn forget(&self, actor: &str, display_name: &str) -> Result<bool> {
        let actor = actor.trim();
        let display_name = display_name.trim();
        if actor.is_empty() {
            return Ok(false);
        }
        let connection = self.connection()?;
        // 遗忘必须原子：中途失败不能留下「已回执忘记、但大厅/摘要还在」。
        let transaction = connection.unchecked_transaction()?;
        // 别名 = 当前 key + ai_chat_actor_names 里登记过的曾用名/显示名。
        // 旧库曾按昵称直接存 facts/turns，只按 UUID key 删会漏。
        let mut aliases = vec![actor.to_string()];
        if !display_name.is_empty() && !aliases.iter().any(|alias| alias == display_name) {
            aliases.push(display_name.to_string());
        }
        {
            let mut stmt = transaction
                .prepare("SELECT name FROM ai_chat_actor_names WHERE actor = ?1 OR name = ?1")?;
            let names = stmt
                .query_map([actor], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for name in names {
                let name = name.trim();
                if !name.is_empty() && !aliases.iter().any(|alias| alias == name) {
                    aliases.push(name.to_string());
                }
            }
        }

        let mut facts = 0usize;
        let mut turns = 0usize;
        let mut actor_summaries = 0usize;
        for alias in &aliases {
            facts += transaction.execute("DELETE FROM ai_chat_facts WHERE actor = ?1", [alias])?;
            turns += transaction.execute("DELETE FROM ai_chat_turns WHERE actor = ?1", [alias])?;
            actor_summaries += transaction.execute(
                "DELETE FROM ai_chat_summaries WHERE scope = ?1",
                [Self::actor_scope(alias)],
            )?;
        }
        // 大厅行：按受众 key 删除；旧库 actor_key='' 的行没有归属，只能按
        // speaker 在别名集合内做保守删除（BOT 旧行无法归属，保持原样）。
        let hall = transaction.execute(
            "DELETE FROM ai_chat_hall_turns WHERE actor_key = ?1",
            [actor],
        )?;
        let legacy_hall = if aliases.len() == 1 {
            transaction.execute(
                "DELETE FROM ai_chat_hall_turns WHERE actor_key = '' AND speaker = ?1",
                [&aliases[0]],
            )?
        } else {
            let placeholders = vec!["?"; aliases.len()].join(",");
            let sql = format!(
                "DELETE FROM ai_chat_hall_turns WHERE actor_key = '' AND speaker IN ({placeholders})"
            );
            transaction.execute(&sql, rusqlite::params_from_iter(aliases.iter()))?
        };
        // legacy BOT 行没有 actor_key，无法归属；遗忘时保守清空（隐私优先）。
        let legacy_bot_hall = transaction.execute(
            "DELETE FROM ai_chat_hall_turns WHERE actor_key = '' AND role = 'bot'",
            [],
        )?;
        let actor_names =
            transaction.execute("DELETE FROM ai_chat_actor_names WHERE actor = ?1", [actor])?;
        // legacy 表：功能已废弃、不再读取，但磁盘留存完整回答；遗忘时直接清空。
        let bot_lines = transaction.execute("DELETE FROM ai_chat_bot_lines", [])?;

        let removed_before_summary = facts
            + turns
            + actor_summaries
            + hall
            + legacy_hall
            + legacy_bot_hall
            + actor_names
            + bot_lines;
        // hall 摘要无法按人拆分；只有确有删除时才整体清空，避免无谓清掉
        // 所有人的中期大厅上下文。
        let hall_summary = if removed_before_summary > 0 {
            transaction.execute(
                "DELETE FROM ai_chat_summaries WHERE scope = ?1",
                [Self::hall_scope()],
            )?
        } else {
            0
        };
        transaction.commit()?;

        let removed = removed_before_summary + hall_summary;
        if removed > 0 {
            // trace 是进程内完整报文环形缓冲，无 actor 关联，只能保守全清。
            crate::ai_trace::clear();
            // 审计只记数量，不记昵称/正文/key。
            log::info!(
                "AI 记忆遗忘级联：facts {facts}、turns {turns}、个人摘要 {actor_summaries}、大厅行 {hall}+{legacy_hall}、legacy BOT hall 行 {legacy_bot_hall}、曾用名 {actor_names}、legacy bot 表 {bot_lines}、大厅摘要 {hall_summary}；trace 已清空"
            );
        }
        Ok(removed > 0)
    }

    /// 记录大厅里的一句话（所有人在大厅说的，全局共享）。
    ///
    /// 与个人记忆分开存：大厅上下文是**所有玩家共享**的，谁都能引用；
    /// 个人记忆是长久的，跟着具体的人走。
    /// 记录大厅里的一句话。
    ///
    /// `role` 区分玩家发言与 BOT 自己的发言；`actor_key` 记录受众：玩家行是
    /// 提问者 key，BOT 行是回复对象 key。`hall_context` 据此避免当前玩家的
    /// 问答在个人 messages 与 hall 里重复；空串是旧库遗留行，仍对所有人可见。
    pub(crate) fn record_hall_line(
        &self,
        speaker: &str,
        text: &str,
        role: HallSpeakerRole,
        actor_key: &str,
        budget: usize,
        now_ms: i64,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let tokens = estimate_tokens(text);
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO ai_chat_hall_turns(speaker, text, tokens, added_at, role, actor_key)              VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![speaker, text, tokens as i64, now_ms, role.as_str(), actor_key.trim()],
        )?;
        prune_hall_hard_cap(&connection, budget)?;
        Ok(())
    }

    /// 迟滞压缩的核心判定：超过**触发水位**才压，并且一次压到**目标水位**。
    ///
    /// 返回需要移出的条目数；0 表示不用压缩。
    ///
    /// 旧实现把"保留 10%"同时当触发线和回落目标（keep = budget*10%），
    /// 于是压完立刻又贴着触发线，下一轮必定再触发——线上表现为每轮一次摘要调用。
    /// 现在两个水位分开：82% 触发、15% 回收，压缩间隔约 320 轮。
    fn overflow_count(&self, table: &str, actor: Option<&str>, budget: usize) -> Result<usize> {
        let trigger = budget * COMPRESS_TRIGGER_PERCENT / 100;
        let target = budget * COMPRESS_TARGET_PERCENT / 100;
        let connection = self.connection()?;
        let sql = match actor {
            Some(_) => {
                format!("SELECT tokens, id FROM {table} WHERE actor = ?1 ORDER BY id ASC")
            }
            None => format!("SELECT tokens, id FROM {table} ORDER BY id ASC"),
        };
        let mut stmt = connection.prepare(&sql)?;
        let mut rows = match actor {
            Some(actor) => stmt.query(params![actor])?,
            None => stmt.query([])?,
        };
        let mut total = 0usize;
        let mut counts: Vec<(Option<i64>, usize)> = Vec::new();
        while let Some(row) = rows.next()? {
            // 行 id 只用于「跳过最新一条」，不参与外部输入拼接。
            let id: Option<i64> = row.get(1).ok();
            let tokens: i64 = row.get(0)?;
            total += tokens as usize;
            counts.push((id, tokens as usize));
        }
        if total <= trigger {
            return Ok(0);
        }
        // 永远不压掉最新一条：它是"当前正在聊的话题"，压进摘要会让模型
        // 对刚说过的话失忆。prune_actor 也有同样的保护。
        let newest = counts.iter().filter_map(|(id, _)| *id).max();
        let mut remaining = total;
        let mut taken = 0usize;
        for (id, tokens) in counts {
            if remaining <= target {
                break;
            }
            if id.is_some() && id == newest {
                continue;
            }
            remaining -= tokens;
            taken += 1;
        }
        Ok(taken)
    }
    /// 读取某个 scope 的中期摘要。
    fn read_summary(&self, scope: &str) -> Result<Option<String>> {
        let connection = self.connection()?;
        let summary = connection
            .query_row(
                "SELECT summary FROM ai_chat_summaries WHERE scope = ?1",
                [scope],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(summary.filter(|text| !text.trim().is_empty()))
    }

    fn write_summary(&self, scope: &str, summary: &str, now_ms: i64) -> Result<()> {
        let summary = summary.trim();
        let connection = self.connection()?;
        if summary.is_empty() {
            connection.execute("DELETE FROM ai_chat_summaries WHERE scope = ?1", [scope])?;
            return Ok(());
        }
        // 摘要也要受预算约束：它和原文一样占上下文，无上限增长等于没做压缩。
        // 预算靠 config 传入，这里只做硬上限保护（1/4 的原文预算）。
        let tokens = estimate_tokens(summary);
        connection.execute(
            concat!(
                "INSERT INTO ai_chat_summaries(scope, summary, tokens, updated_at) ",
                "VALUES (?1, ?2, ?3, ?4) ",
                "ON CONFLICT(scope) DO UPDATE SET summary = excluded.summary, ",
                "tokens = excluded.tokens, updated_at = excluded.updated_at"
            ),
            params![scope, summary, tokens as i64, now_ms],
        )?;
        Ok(())
    }

    /// 只读取最旧的一批原文下标与内容，**不删除**。
    ///
    /// 删除必须等到摘要成功写回之后（见 `consume_compressed`）。
    /// 原来这里是「取出即删」：模型调用失败或返回空摘要时，原文已经没了、
    /// 摘要也没写，那段记忆永久消失——与调用点注释「原文仍在库里，下一轮再试」
    /// 完全矛盾。
    ///
    /// `table` / `column` 只接受内部常量（不来自外部输入），拼接安全。
    fn take_oldest(
        &self,
        table: &str,
        column: &str,
        actor: Option<&str>,
        count: usize,
    ) -> Result<Vec<(i64, String)>> {
        let connection = self.connection()?;
        let mut out = Vec::new();
        let mut ids = Vec::new();
        {
            let sql = match actor {
                Some(_) => format!(
                    "SELECT id, {column} FROM {table} WHERE actor = ?1 ORDER BY id ASC LIMIT ?2"
                ),
                None => format!("SELECT id, {column} FROM {table} ORDER BY id ASC LIMIT ?1"),
            };
            let mut stmt = connection.prepare(&sql)?;
            let mut rows = match actor {
                Some(actor) => stmt.query(params![actor, count as i64])?,
                None => stmt.query(params![count as i64])?,
            };
            while let Some(row) = rows.next()? {
                ids.push(row.get::<_, i64>(0)?);
                out.push(row.get::<_, String>(1)?);
            }
        }
        Ok(ids.into_iter().zip(out).collect())
    }

    /// 压缩成功后删除已消费的原文。
    ///
    /// 只有摘要确实写回之后才调用：先删后写会在模型失败时永久丢记忆。
    pub(crate) fn consume_compressed(&self, table: &str, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        for id in ids {
            transaction.execute(&format!("DELETE FROM {table} WHERE id = ?1"), [id])?;
        }
        transaction.commit()?;
        Ok(())
    }
    /// 个人记忆：超**触发水位**时算出要压缩哪些原文。
    ///
    /// 分层：原文占用 >82% 时压到 15%，其余连同旧摘要一起重新压缩成新摘要。
    pub(crate) fn plan_actor_compression(
        &self,
        actor: &str,
        budget: usize,
    ) -> Result<Option<CompressionPlan>> {
        let overflow = self.overflow_count("ai_chat_turns", Some(actor), budget)?;
        if overflow == 0 {
            return Ok(None);
        }
        let scope = Self::actor_scope(actor);
        let previous_summary = self.read_summary(&scope)?.unwrap_or_default();
        // 取「问 + 答」整段：只压问题会把机器人的回答丢掉，
        // 而回答里往往含结论（点歌结果、约定内容），丢了等于记忆缺一半。
        let rows = self.take_oldest(
            "ai_chat_turns",
            "question || ' → ' || answer",
            Some(actor),
            overflow,
        )?;
        let source_ids = rows.iter().map(|(id, _)| *id).collect();
        let records = rows.into_iter().map(|(_, text)| text).collect();
        Ok(Some(CompressionPlan {
            scope,
            previous_summary,
            records,
            source_table: "ai_chat_turns",
            source_ids,
        }))
    }

    /// 大厅记忆：同样的迟滞分层，但作用于共享的大厅原文。
    pub(crate) fn plan_hall_compression(&self, budget: usize) -> Result<Option<CompressionPlan>> {
        let overflow = self.overflow_count("ai_chat_hall_turns", None, budget)?;
        if overflow == 0 {
            return Ok(None);
        }
        let scope = Self::hall_scope().to_string();
        let previous_summary = self.read_summary(&scope)?.unwrap_or_default();
        // 带说话人：大厅是多人共享的记忆，压掉名字后「谁说了什么」就没了。
        let rows = self.take_oldest(
            "ai_chat_hall_turns",
            "speaker || '：' || text",
            None,
            overflow,
        )?;
        let source_ids = rows.iter().map(|(id, _)| *id).collect();
        let records = rows.into_iter().map(|(_, text)| text).collect();
        Ok(Some(CompressionPlan {
            scope,
            previous_summary,
            records,
            source_table: "ai_chat_hall_turns",
            source_ids,
        }))
    }

    /// 写回压缩结果。
    ///
    /// **绝不截断摘要正文**：摘要本身就是信息，砍掉尾巴等于丢记忆。
    /// 容量靠下一轮压缩控制——摘要变长意味着下一轮会更早触发重新压缩。
    /// 若模型返回空摘要，保留旧摘要，不能把已有记忆清空。
    /// 摘要写回成功后才删除已消费的原文。
    ///
    /// 顺序是硬要求：先删后写会在模型失败时把那段记忆永久抹掉。
    pub(crate) fn apply_compression(
        &self,
        plan: &CompressionPlan,
        summary: &str,
        summary_budget: usize,
        now_ms: i64,
    ) -> Result<()> {
        let summary = summary.trim();
        if summary.is_empty() {
            // 原文一条都没删，下轮会重新规划。
            log::warn!(
                "记忆压缩返回空摘要，本次保留旧摘要与原文（范围：{}）",
                plan.scope
            );
            return Ok(());
        }
        // 摘要硬上限：面板上的 summary_budget 以前只存在于配置里，从不生效。
        // 不设上限时一次坏摘要会长期占满提示词，把原文挤出去。
        let summary = clip_to_budget(summary, summary_budget);
        self.write_summary(&plan.scope, &summary, now_ms)?;
        self.consume_compressed(plan.source_table, &plan.source_ids)
    }

    /// 列出所有有记忆的玩家及其规模（面板用）。
    ///
    /// 事实与原文分开计数：面板要能一眼看出「这个人被记了什么、多不多」。
    pub(crate) fn list_actors(&self) -> Result<Vec<ActorMemorySummary>> {
        let connection = self.connection()?;
        let mut stmt = connection.prepare(
            "SELECT actor,
                    (SELECT COUNT(*) FROM ai_chat_facts WHERE actor = t.actor),
                    (SELECT COUNT(*) FROM ai_chat_turns WHERE actor = t.actor),
                    (SELECT COALESCE(SUM(tokens), 0) FROM ai_chat_facts WHERE actor = t.actor)
                  + (SELECT COALESCE(SUM(tokens), 0) FROM ai_chat_turns WHERE actor = t.actor)
             FROM (SELECT DISTINCT actor FROM ai_chat_facts
                   UNION SELECT DISTINCT actor FROM ai_chat_turns) AS t
             ORDER BY actor ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActorMemorySummary {
                actor: row.get(0)?,
                facts: row.get::<_, i64>(1)? as usize,
                turns: row.get::<_, i64>(2)? as usize,
                tokens: row.get::<_, i64>(3)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 删除某条事实，返回是否确实删掉了。
    pub(crate) fn delete_fact(&self, actor: &str, fact: &str) -> Result<bool> {
        let connection = self.connection()?;
        let removed = connection.execute(
            "DELETE FROM ai_chat_facts WHERE actor = ?1 AND fact = ?2",
            params![actor, fact],
        )?;
        Ok(removed > 0)
    }

    /// 清空大厅共享记忆（原文 + 摘要）。
    pub(crate) fn forget_hall(&self) -> Result<usize> {
        let connection = self.connection()?;
        let lines = connection.execute("DELETE FROM ai_chat_hall_turns", [])?;
        connection.execute(
            "DELETE FROM ai_chat_summaries WHERE scope = ?1",
            [Self::hall_scope()],
        )?;
        Ok(lines)
    }

    /// 大厅原文条数与 token 规模。
    pub(crate) fn hall_stats(&self) -> Result<(usize, usize)> {
        let connection = self.connection()?;
        let row: (i64, i64) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens), 0) FROM ai_chat_hall_turns",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((row.0 as usize, row.1 as usize))
    }
    /// 记忆摘要：事实条数与最近一次提问。
    pub(crate) fn summary(&self, actor: &str) -> Result<(usize, Option<String>)> {
        let connection = self.connection()?;
        let facts: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM ai_chat_facts WHERE actor = ?1",
                [actor],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let last = connection
            .query_row(
                "SELECT question FROM ai_chat_turns WHERE actor = ?1 ORDER BY id DESC LIMIT 1",
                [actor],
                |row| row.get(0),
            )
            .optional()?;
        Ok((facts as usize, last))
    }
}

/// 按单人预算淘汰最旧的**对话原文**。
///
/// 只动 turns：facts 有自己的预算（[prune_facts]）。以前两者共用一个池，
/// 超预算时会先把最旧的事实删掉，与"事实是远期权威结论"的定位相反。
///
/// 传进来的是**落库预算**（默认 [ACTOR_TOKEN_BUDGET] = 96K）。这里再乘
/// [HARD_CAP_PERCENT]（150%）得到真正的删除阈值：它必须**高于**压缩触发线
/// （82% = 78.72K），否则压缩还没来得及跑、每轮写入就先把最旧一行删掉，
/// 历史会退化成滑动窗口。正常路径永远先由压缩把原文搬进摘要，这个兜底只在
/// 压缩连续失败、占用真的涨到 144K 时才删行。
fn prune_actor(connection: &Connection, actor: &str, budget: usize) -> Result<()> {
    let cap = budget * HARD_CAP_PERCENT / 100;
    let total: i64 = connection.query_row(
        "SELECT COALESCE((SELECT SUM(tokens) FROM ai_chat_turns WHERE actor = ?1), 0)",
        [actor],
        |row| row.get(0),
    )?;
    let mut remaining = total;
    if remaining as usize <= cap {
        return Ok(());
    }
    // 始终保留最新一条：否则极小预算会把刚写入的内容也删掉，记忆永远为空。
    let newest_turn: Option<i64> = connection
        .query_row(
            "SELECT id FROM ai_chat_turns WHERE actor = ?1 ORDER BY id DESC LIMIT 1",
            [actor],
            |row| row.get(0),
        )
        .optional()?;
    let mut stale_turns = Vec::new();
    {
        let mut stmt = connection
            .prepare("SELECT id, tokens FROM ai_chat_turns WHERE actor = ?1 ORDER BY id ASC")?;
        let mut rows = stmt.query([actor])?;
        while let Some(row) = rows.next()? {
            if remaining as usize <= cap {
                break;
            }
            let id: i64 = row.get(0)?;
            if Some(id) == newest_turn {
                continue;
            }
            let tokens: i64 = row.get(1)?;
            remaining -= tokens;
            stale_turns.push(id);
        }
    }
    for id in stale_turns {
        connection.execute("DELETE FROM ai_chat_turns WHERE id = ?1", [id])?;
    }
    Ok(())
}

/// 事实层自己的淘汰：先按条数、再按 token。
///
/// 只删**最久未被更新**的那条：事实的 added_at 会在重复写入时刷新，
/// 所以"最久没被提到"最接近"已经不重要"，比按插入顺序删更稳。
fn prune_facts(connection: &Connection, actor: &str) -> Result<()> {
    let total: i64 = connection.query_row(
        "SELECT COALESCE((SELECT SUM(tokens) FROM ai_chat_facts WHERE actor = ?1), 0)",
        [actor],
        |row| row.get(0),
    )?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM ai_chat_facts WHERE actor = ?1",
        [actor],
        |row| row.get(0),
    )?;
    let mut excess = count.saturating_sub(MAX_FACTS_PER_ACTOR as i64);
    let mut over_tokens = total.saturating_sub(ACTOR_FACTS_BUDGET as i64);
    if excess <= 0 && over_tokens <= 0 {
        return Ok(());
    }
    let mut victims = Vec::new();
    {
        let mut stmt = connection.prepare(
            "SELECT rowid, tokens FROM ai_chat_facts WHERE actor = ?1
             ORDER BY added_at ASC, rowid ASC",
        )?;
        let mut rows = stmt.query([actor])?;
        while let Some(row) = rows.next()? {
            if excess <= 0 && over_tokens <= 0 {
                break;
            }
            let id: i64 = row.get(0)?;
            let tokens: i64 = row.get(1)?;
            victims.push(id);
            excess -= 1;
            over_tokens -= tokens;
        }
    }
    for id in victims {
        connection.execute("DELETE FROM ai_chat_facts WHERE rowid = ?1", [id])?;
    }
    Ok(())
}

/// 大厅原文的硬上限兜底：压缩失败/模型不可用时仍不能让表无限增长。
///
/// 只在超过 [HARD_CAP_PERCENT] 预算时动手，且保留最新一条——
/// 正常压缩路径永远先触发（82%），这里是最后一道保险。
fn prune_hall_hard_cap(connection: &Connection, budget: usize) -> Result<()> {
    let cap = budget * HARD_CAP_PERCENT / 100;
    let total: i64 = connection.query_row(
        "SELECT COALESCE((SELECT SUM(tokens) FROM ai_chat_hall_turns), 0)",
        [],
        |row| row.get(0),
    )?;
    if total as usize <= cap {
        return Ok(());
    }
    let mut remaining = total as usize;
    let mut victims = Vec::new();
    {
        let mut stmt =
            connection.prepare("SELECT id, tokens FROM ai_chat_hall_turns ORDER BY id ASC")?;
        let mut rows = stmt.query([])?;
        // 至少留一条最新记录。
        let mut collected = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let tokens: usize = row.get::<_, i64>(1)? as usize;
            collected.push((id, tokens));
        }
        if collected.len() <= 1 {
            return Ok(());
        }
        let last = collected.pop().map(|(id, _)| id);
        for (id, tokens) in collected {
            if remaining <= cap {
                break;
            }
            remaining -= tokens;
            victims.push(id);
        }
        debug_assert!(last.is_some());
    }
    for id in victims {
        connection.execute("DELETE FROM ai_chat_hall_turns WHERE id = ?1", [id])?;
    }
    Ok(())
}

/// 把摘要裁到指定 token 预算内。
///
/// 按句子边界裁：摘要被拦腰截断会变成半句话，模型读起来比没有更糟。
/// 一条句子都放不下时才退化成按字符截断。
fn clip_to_budget(text: &str, budget: usize) -> String {
    if budget == 0 || estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let mut out = String::new();
    for sentence in text.split_inclusive(['。', '！', '？', '\n', '.', '!', '?']) {
        let candidate_len = out.len() + sentence.len();
        let candidate = &text[..candidate_len];
        if estimate_tokens(candidate) > budget {
            break;
        }
        out.push_str(sentence);
    }
    if out.trim().is_empty() {
        // 单句就超预算：按字符退让，保证不返回空（空会被判为失败、下轮重试）。
        let mut clipped = String::new();
        for ch in text.chars() {
            clipped.push(ch);
            if estimate_tokens(&clipped) > budget.saturating_sub(1).max(1) {
                break;
            }
        }
        return clipped;
    }
    out.trim_end().to_string()
}

/// 粗略 token 估算：ASCII 约 0.3，其余（中日韩等）约 1。
///
/// 这里只需要一个稳定、可解释的容量单位，不追求与具体模型 tokenizer 对齐。
pub(crate) fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0.0f64;
    for ch in text.chars() {
        tokens += if ch.is_ascii() { 0.3 } else { 1.0 };
    }
    tokens.ceil() as usize
}

/// 事实抽取的用户消息。
///
/// 两个地方共用：单轮增量抽取，以及历史重放清洗。规则必须完全一致，
/// 否则清洗出来的结果和日常增量抽取的口径会对不上。
///
/// 三条硬约束，每条都对应一次线上事故：
///
/// 1. **事实只能来自玩家原话**。BOT 一句带疑问语气的猜测（「是古风吧喵？」）
///    曾被当成玩家信息写入，之后 BOT 反复复读自己的编造，三轮后变成
///    「确定事实」并污染后续所有回答。
/// 2. **必须告诉模型「谁在说话」**。原来只传裸正文，模型不知道主语是谁，
///    于是把「玩家称呼助手为橘子」写反成「玩家自称橘子」，也把
///    大厅里别人的名字记到本人头上。
/// 3. **称谓要分方向**。玩家报自己名字 vs 玩家给 BOT 起名字是两回事，
///    合在一个「称呼」里必然写反。
pub(crate) fn fact_extraction_prompt(
    actor: &str,
    existing: &str,
    question: &str,
    answer: &str,
) -> String {
    format!(
        "当前玩家昵称：{actor}
         已有事实（按时间从旧到新）：
{existing}

         玩家本轮的原话：
{question}

         你的回复（**仅用于理解上下文**，其中的内容不是你听到玩家说过的话，         绝不可据此生成事实）：
{answer}

         任务：只从「玩家本轮的原话」中抽取关于 {actor} 本人、值得长期记住的稳定信息。
         
         **称谓必须分方向，写错方向比不写更糟：**
         - 玩家自报名字、自我介绍、要求你改叫他某个名字 → 写成「玩家自称：X」
         - 玩家给你起的名字、说该怎么叫你 → 写成「玩家称呼助手：Y」
         - 玩家提到第三个人的名字 → 属于别人，一律不写进 {actor} 的事实
         - 「我叫玩家丙」只能产出「玩家自称：玩家丙」，绝不能写成「玩家称呼助手为玩家丙」
         
         判定规则：
         1. 玩家**明确陈述**的事实才可写入 facts；
         2. 玩家只是提问、寒暄、让你做事，一律不写入；
         3. 你的回复里出现的内容（包括猜测、反问、举例、你提到的别人名字）一律不写入；
         4. 带猜测或不确定语气的表述（如「是…吧？」「可能」「那些」）不写入；
         5. 已有事实与本轮玩家陈述冲突时，把被推翻的那条原句放进 remove；         只推翻**同一类别**的旧事实——新的偏好不会推翻旧的「玩家自称」；
         6. 纯寒暄或没有新信息时，facts 与 remove 都返回空数组。
         
         严禁把其他玩家说过的名字或信息写成 {actor} 的事实；判断不了是谁的就不写。
         只返回 JSON：{{\"facts\":[\"…\"],\"remove\":[\"…\"]}}。
         每条以「玩家」为主语，不要写「助手」「你」；称谓类必须带「玩家自称：」或「玩家称呼助手：」前缀。"
    )
}

/// 抽取结果的完整解析：新增事实 + 需要删除的旧事实原句。
///
/// 只看 facts 会让改名、改称呼这类更新变成「再追加一条」，新旧两条并存，
/// 模型随后在新旧之间随机挑一条——表现为「没有使用最新信息」。
pub(crate) fn parse_fact_update(reply: &str) -> (Vec<String>, Vec<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(reply.trim()) else {
        return (Vec::new(), Vec::new());
    };
    let collect = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let facts = collect("facts")
        .into_iter()
        .filter(|fact| fact.chars().count() <= MAX_FACT_CHARS)
        .collect();
    (facts, collect("remove"))
}
