//! 命令回复模板：独立 revision 与 CAS 保存。
//!
//! 命令回复表（`command_reply_meta` / `command_replies`）与配置三表同库，但
//! **revision 完全独立**：改回复模板不该让配置历史版本变多，反之亦然。
//!
//! 空向量用 `ordinal = -1` 的哨兵行表示「该命令被显式静音」，与「从未保存过该
//! 命令」区分开；读取时 ordinals 必须从 0 连续，存储被手工改坏就明确报错，而不是
//! 悄悄把回复贴到错误的命令上。

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use super::ConfigStore;
use super::types::StoredReply;

impl ConfigStore {
    /// 当前命令回复存储的独立 revision。
    pub fn reply_revision(&self) -> Result<u64> {
        let revision: i64 = self.connection.query_row(
            "SELECT revision FROM command_reply_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(revision).context("命令回复 revision 为负数")
    }

    /// 读取命令回复；每个向量按持久化 ordinal 排序。
    pub fn reply_templates(&self) -> Result<BTreeMap<String, Vec<StoredReply>>> {
        let mut statement = self.connection.prepare(
            "SELECT command_id, ordinal, key, text FROM command_replies
                 ORDER BY command_id, ordinal",
        )?;
        let mut rows = statement.query([])?;
        let mut commands = BTreeMap::<String, Vec<StoredReply>>::new();
        let mut empty_overrides = BTreeSet::new();
        while let Some(row) = rows.next()? {
            let command_id: String = row.get(0)?;
            let ordinal: i64 = row.get(1)?;
            let key: String = row.get(2)?;
            let text: String = row.get(3)?;
            if ordinal == -1 {
                if key.is_empty()
                    && text.is_empty()
                    && commands.get(&command_id).is_none_or(Vec::is_empty)
                    && empty_overrides.insert(command_id.clone())
                {
                    continue;
                }
                bail!("命令回复存储包含无效的空列表标记");
            }
            if empty_overrides.contains(&command_id) {
                bail!("空列表标记不能与普通命令回复并存");
            }
            let replies = commands.entry(command_id).or_default();
            if ordinal < 0 || ordinal as usize != replies.len() {
                bail!("命令回复存储的 ordinal 不连续");
            }
            replies.push(StoredReply { key, text });
        }
        for command_id in empty_overrides {
            commands.insert(command_id, Vec::new());
        }
        Ok(commands)
    }

    /// CAS 替换已提交命令的回复列表；未提交命令不变，空向量保存为空覆盖。
    pub fn save_reply_templates(
        &mut self,
        base_revision: u64,
        commands: &BTreeMap<String, Vec<StoredReply>>,
    ) -> Result<u64> {
        if commands.is_empty() {
            bail!("命令回复保存至少需要一个命令");
        }
        for (command_id, replies) in commands {
            if command_id.trim().is_empty() {
                bail!("命令回复 command_id 不能为空");
            }
            let mut keys = BTreeSet::new();
            for reply in replies {
                if reply.key.trim().is_empty() {
                    bail!("命令回复 key 不能为空");
                }
                if !keys.insert(reply.key.as_str()) {
                    bail!("命令 {command_id} 包含重复的回复 key: {}", reply.key);
                }
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored_revision: i64 = transaction.query_row(
            "SELECT revision FROM command_reply_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        if u64::try_from(stored_revision).context("命令回复 revision 为负数")? != base_revision
        {
            bail!("命令回复 revision 冲突，请刷新后重试");
        }
        for (command_id, replies) in commands {
            transaction.execute(
                "DELETE FROM command_replies WHERE command_id = ?1",
                rusqlite::params![command_id],
            )?;
            if replies.is_empty() {
                transaction.execute(
                    "INSERT INTO command_replies (command_id, ordinal, key, text)
                         VALUES (?1, -1, '', '')",
                    rusqlite::params![command_id],
                )?;
            } else {
                for (ordinal, reply) in replies.iter().enumerate() {
                    let ordinal = i64::try_from(ordinal).context("回复列表过长")?;
                    transaction.execute(
                        "INSERT INTO command_replies (command_id, ordinal, key, text)
                             VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![command_id, ordinal, reply.key, reply.text],
                    )?;
                }
            }
        }
        let next_revision = stored_revision
            .checked_add(1)
            .context("命令回复 revision 溢出")?;
        transaction.execute(
            "UPDATE command_reply_meta SET revision = ?1 WHERE id = 1",
            rusqlite::params![next_revision],
        )?;
        transaction.commit()?;
        u64::try_from(next_revision).context("命令回复 revision 为负数")
    }

    pub(super) fn ensure_reply_schema(connection: &rusqlite::Connection) -> Result<()> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS command_reply_meta (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    revision INTEGER NOT NULL CHECK (revision >= 0)
                 );
                 INSERT OR IGNORE INTO command_reply_meta (id, revision) VALUES (1, 0);
                 CREATE TABLE IF NOT EXISTS command_replies (
                    command_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    key TEXT NOT NULL,
                    text TEXT NOT NULL,
                    PRIMARY KEY (command_id, ordinal),
                    UNIQUE (command_id, key),
                    CHECK ((ordinal = -1 AND key = '' AND text = '') OR
                           (ordinal >= 0 AND key <> ''))
                 );",
        )?;
        Ok(())
    }
}
