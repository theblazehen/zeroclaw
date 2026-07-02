use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::path::Path;
use zeroclaw_config::schema::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: String,
    pub owner_agent_id: String,
    pub title: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub heartbeat_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub result_summary: Option<String>,
    pub error_message: Option<String>,
}

fn db_path(config: &Config) -> std::path::PathBuf {
    config.data_dir.join("tasks/tasks.db")
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create task ledger dir {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open task ledger {}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            task_id TEXT NOT NULL,
            owner_agent_id TEXT NOT NULL,
            title TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            heartbeat_at TEXT,
            last_progress_at TEXT,
            result_summary TEXT,
            error_message TEXT,
            metadata TEXT NOT NULL DEFAULT '{}',
            PRIMARY KEY (task_id, owner_agent_id)
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_owner_status ON tasks(owner_agent_id, status);
        CREATE TABLE IF NOT EXISTS task_events (
            event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            emitted_at TEXT NOT NULL,
            summary TEXT,
            payload TEXT NOT NULL DEFAULT '{}'
        );",
    )?;
    Ok(conn)
}

pub fn upsert_task(
    config: &Config,
    owner_agent_id: &str,
    task_id: &str,
    title: &str,
    status: &str,
    result_summary: Option<&str>,
    error_message: Option<&str>,
) -> Result<()> {
    let conn = open(&db_path(config))?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tasks (
            task_id, owner_agent_id, title, status, created_at, updated_at,
            heartbeat_at, last_progress_at, result_summary, error_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?5, ?5, ?6, ?7)
        ON CONFLICT(task_id, owner_agent_id) DO UPDATE SET
            title = excluded.title,
            status = excluded.status,
            updated_at = excluded.updated_at,
            heartbeat_at = excluded.heartbeat_at,
            last_progress_at = excluded.last_progress_at,
            result_summary = excluded.result_summary,
            error_message = excluded.error_message",
        params![
            task_id,
            owner_agent_id,
            title,
            status,
            now,
            result_summary,
            error_message
        ],
    )?;
    conn.execute(
        "INSERT INTO task_events(task_id, event_type, emitted_at, summary) VALUES (?1, ?2, ?3, ?4)",
        params![task_id, status, now, result_summary.or(error_message)],
    )?;
    Ok(())
}

pub fn heartbeat(config: &Config, owner_agent_id: &str, task_id: &str) -> Result<()> {
    let conn = open(&db_path(config))?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE tasks SET heartbeat_at = ?1, updated_at = ?1 WHERE task_id = ?2 AND owner_agent_id = ?3",
        params![now, task_id, owner_agent_id],
    )?;
    Ok(())
}

pub fn list_tasks(config: &Config, owner_agent_id: Option<&str>) -> Result<Vec<TaskRecord>> {
    let conn = open(&db_path(config))?;
    let sql = if owner_agent_id.is_some() {
        "SELECT task_id, owner_agent_id, title, status, created_at, updated_at,
                heartbeat_at, last_progress_at, result_summary, error_message
         FROM tasks WHERE owner_agent_id = ?1 ORDER BY updated_at DESC"
    } else {
        "SELECT task_id, owner_agent_id, title, status, created_at, updated_at,
                heartbeat_at, last_progress_at, result_summary, error_message
         FROM tasks ORDER BY updated_at DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok(TaskRecord {
            task_id: row.get(0)?,
            owner_agent_id: row.get(1)?,
            title: row.get(2)?,
            status: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
            heartbeat_at: row.get(6)?,
            last_progress_at: row.get(7)?,
            result_summary: row.get(8)?,
            error_message: row.get(9)?,
        })
    };
    let rows = if let Some(owner) = owner_agent_id {
        stmt.query_map(params![owner], map)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        stmt.query_map([], map)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn upsert_and_list_task_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();
        upsert_task(
            &config,
            "main",
            "task-1",
            "Check mail",
            "in_progress",
            None,
            None,
        )
        .unwrap();
        heartbeat(&config, "main", "task-1").unwrap();
        let tasks = list_tasks(&config, Some("main")).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "task-1");
        assert_eq!(tasks[0].status, "in_progress");
        assert!(tasks[0].heartbeat_at.is_some());
    }

    #[test]
    fn same_task_id_from_different_agents_does_not_overwrite() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.data_dir = tmp.path().to_path_buf();

        upsert_task(
            &config,
            "main",
            "msg-123",
            "WhatsApp message",
            "in_progress",
            None,
            None,
        )
        .unwrap();
        upsert_task(
            &config,
            "mail_triage",
            "msg-123",
            "Cron job output",
            "in_progress",
            None,
            None,
        )
        .unwrap();

        let main_tasks = list_tasks(&config, Some("main")).unwrap();
        let cron_tasks = list_tasks(&config, Some("mail_triage")).unwrap();

        // If task_id is globally unique, both agents should see their own task.
        // If task_id collides, one agent's task is overwritten by the other.
        assert_eq!(
            main_tasks.len(),
            1,
            "agent 'main' should still see its task with the same task_id"
        );
        assert_eq!(
            cron_tasks.len(),
            1,
            "agent 'mail_triage' should also see its task with the same task_id"
        );
    }
}
