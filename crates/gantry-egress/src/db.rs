//! `egress.db`（SQLite，WAL）：令牌登记（只存哈希）、用量记录、调用墓碑、在途登记、
//! 已应用配置视图与暂存元数据（`docs/modules/egress.md` §7.2）。
//!
//! 行以 JSON 存放；表里没有令牌原文，也没有任何 secret 值（值只在 `secrets` 实例里）。

use std::path::Path;

use gantry_core::egress::UsageRecord;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("egress.db: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("egress.db row is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (job_id TEXT PRIMARY KEY, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS svc_tokens (svc_token_id TEXT PRIMARY KEY, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS usage (seq INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS calls (call_id TEXT PRIMARY KEY, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS inflight (id INTEGER PRIMARY KEY, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS staged (staged_id TEXT PRIMARY KEY, row TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// 表名白名单，防止拼接 SQL 时混入外部输入。
#[derive(Clone, Copy, Debug)]
pub enum Table {
    Jobs,
    SvcTokens,
    Calls,
    Inflight,
    Staged,
}

impl Table {
    fn sql(self) -> (&'static str, &'static str) {
        match self {
            Self::Jobs => ("jobs", "job_id"),
            Self::SvcTokens => ("svc_tokens", "svc_token_id"),
            Self::Calls => ("calls", "call_id"),
            Self::Inflight => ("inflight", "id"),
            Self::Staged => ("staged", "staged_id"),
        }
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// 一个 SQLite 事务；闭包内的写入要么全部落盘，要么全部不生效。
    pub fn tx<T>(&mut self, f: impl FnOnce(&Tx<'_>) -> Result<T, DbError>) -> Result<T, DbError> {
        let tx = self.conn.transaction()?;
        let out = f(&Tx { tx: &tx })?;
        tx.commit()?;
        Ok(out)
    }

    pub fn load<T: DeserializeOwned>(&self, table: Table) -> Result<Vec<(String, T)>, DbError> {
        let (name, key) = table.sql();
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {key}, row FROM {name} ORDER BY {key}"))?;
        let rows = stmt.query_map([], |row| {
            let key: rusqlite::types::Value = row.get(0)?;
            let key = match key {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Text(s) => s,
                _ => String::new(),
            };
            Ok((key, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, json) = row?;
            out.push((key, serde_json::from_str(&json)?));
        }
        Ok(out)
    }

    pub fn load_usage(&self) -> Result<Vec<UsageRecord>, DbError> {
        let mut stmt = self.conn.prepare("SELECT row FROM usage ORDER BY seq")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn meta<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, DbError> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(DbError::from)
    }
}

pub struct Tx<'a> {
    tx: &'a rusqlite::Transaction<'a>,
}

impl Tx<'_> {
    pub fn put<T: Serialize>(&self, table: Table, key: &str, row: &T) -> Result<(), DbError> {
        let (name, column) = table.sql();
        self.tx.execute(
            &format!("INSERT OR REPLACE INTO {name} ({column}, row) VALUES (?1, ?2)"),
            params![key, serde_json::to_string(row)?],
        )?;
        Ok(())
    }

    pub fn delete(&self, table: Table, key: &str) -> Result<(), DbError> {
        let (name, column) = table.sql();
        self.tx
            .execute(&format!("DELETE FROM {name} WHERE {column} = ?1"), [key])?;
        Ok(())
    }

    pub fn clear(&self, table: Table) -> Result<(), DbError> {
        let (name, _) = table.sql();
        self.tx.execute(&format!("DELETE FROM {name}"), [])?;
        Ok(())
    }

    pub fn insert_usage(&self, record: &UsageRecord) -> Result<(), DbError> {
        self.tx.execute(
            "INSERT INTO usage (seq, at_ms, row) VALUES (?1, ?2, ?3)",
            params![
                record.seq as i64,
                record.at.as_millis(),
                serde_json::to_string(record)?
            ],
        )?;
        Ok(())
    }

    /// 删除 `at` 不晚于 `cutoff_ms` 的用量记录，返回被删的最大 `seq`。
    pub fn prune_usage(&self, cutoff_ms: i64) -> Result<Option<u64>, DbError> {
        let max: Option<i64> = self.tx.query_row(
            "SELECT MAX(seq) FROM usage WHERE at_ms <= ?1",
            [cutoff_ms],
            |row| row.get(0),
        )?;
        self.tx
            .execute("DELETE FROM usage WHERE at_ms <= ?1", [cutoff_ms])?;
        Ok(max.map(|seq| seq as u64))
    }

    pub fn put_meta<T: Serialize>(&self, key: &str, value: &T) -> Result<(), DbError> {
        self.tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }
}
