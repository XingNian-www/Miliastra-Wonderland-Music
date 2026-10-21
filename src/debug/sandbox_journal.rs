//! Restart-safe, bounded archive for completed sandbox runs. No device or HTTP dependency.
use std::path::Path;
use std::sync::Mutex;

use crate::debug_sandbox::SandboxTask;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

pub(crate) const MAX_RECORD_BYTES: usize = 12 * 1024 * 1024;
const MAX_RECORDS: i64 = 64;

pub(crate) struct SandboxJournal {
    connection: Mutex<Connection>,
}

impl SandboxJournal {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let connection = Connection::open(path).map_err(|e| e.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS sandbox_archive_v1 (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                record_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sandbox_tasks_v1 (
                task_id TEXT PRIMARY KEY,
                version INTEGER NOT NULL,
                task_json TEXT NOT NULL
            );",
            )
            .map_err(|e| e.to_string())?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn save_task(&self, task: &SandboxTask) -> Result<(), String> {
        let json = serde_json::to_string(task).map_err(|e| e.to_string())?;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let changed = connection.execute(
            "INSERT INTO sandbox_tasks_v1 (task_id, version, task_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(task_id) DO UPDATE SET version=excluded.version, task_json=excluded.task_json
             WHERE sandbox_tasks_v1.version < excluded.version",
            params![task.id, task.version, json],
        ).map_err(|e| e.to_string())?;
        if changed != 1 {
            return Err("数据库中的任务版本不低于本次版本".into());
        }
        Ok(())
    }

    pub(crate) fn tasks(&self) -> Result<Vec<SandboxTask>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection
            .prepare("SELECT task_json FROM sandbox_tasks_v1 ORDER BY task_id LIMIT 257")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let json = row.map_err(|e| e.to_string())?;
            serde_json::from_str(&json).map_err(|e| e.to_string())
        })
        .collect()
    }

    pub(crate) fn runs(&self) -> Result<Vec<Value>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection
            .prepare("SELECT record_json FROM sandbox_archive_v1 ORDER BY sequence DESC LIMIT 64")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let json = row.map_err(|e| e.to_string())?;
            serde_json::from_str::<Value>(&json)
                .map(|record| record["run"].clone())
                .map_err(|e| e.to_string())
        })
        .collect()
    }

    pub(crate) fn save(&self, run_id: &str, record: &Value) -> Result<(), String> {
        if run_id.is_empty() || run_id.len() > 128 {
            return Err("invalid archive run ID".into());
        }
        let json = serde_json::to_string(record).map_err(|e| e.to_string())?;
        if json.len() > MAX_RECORD_BYTES {
            return Err("sandbox archive record exceeds 12 MiB".into());
        }
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        transaction
            .execute(
                "INSERT INTO sandbox_archive_v1 (run_id, record_json) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET record_json=excluded.record_json",
                params![run_id, json],
            )
            .map_err(|e| e.to_string())?;
        transaction
            .execute(
                "DELETE FROM sandbox_archive_v1 WHERE sequence NOT IN (
                SELECT sequence FROM sandbox_archive_v1 ORDER BY sequence DESC LIMIT ?1
            )",
                [MAX_RECORDS],
            )
            .map_err(|e| e.to_string())?;
        transaction.commit().map_err(|e| e.to_string())
    }

    pub(crate) fn load(&self, run_id: &str) -> Result<Option<Value>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let json: Option<String> = connection
            .query_row(
                "SELECT record_json FROM sandbox_archive_v1 WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        json.map(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_survives_reopen_and_evicts_oldest() {
        let path =
            std::env::temp_dir().join(format!("sandbox-journal-{}.db", uuid::Uuid::new_v4()));
        {
            let archive = SandboxJournal::open(&path).unwrap();
            for i in 0..65 {
                archive
                    .save(
                        &format!("run-{i}"),
                        &serde_json::json!({"evidence": ["png"], "index": i}),
                    )
                    .unwrap();
            }
            archive
                .save("run-64", &serde_json::json!({"evidence": ["updated"]}))
                .unwrap();
        }
        {
            let archive = SandboxJournal::open(&path).unwrap();
            assert_eq!(archive.load("run-0").unwrap(), None);
            assert_eq!(
                archive.load("run-64").unwrap().unwrap()["evidence"][0],
                "updated"
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
