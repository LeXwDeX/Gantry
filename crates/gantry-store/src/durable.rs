//! SQLite 持久层（store.md §7.1、§7.2、§7.9；方案见 #88 的实现方案评论）。
//!
//! 持久化的是**事实源与非投影状态**：`events`（只追加）、`outbox`（主表）、
//! `consumer_cursors`、模块私有表与 `meta`。投影不落表：`open` 时按 `seq` 顺序用
//! 同一套纯折叠函数重放（§7.9"投影随时可丢弃重建"），之后由 `append` 增量维护，
//! 因此内存版与 SQLite 版走同一条代码路径。
//!
//! 写入：`WriteTx::commit` 先调 [`Durable::persist`]，在一个 `BEGIN IMMEDIATE`
//! 事务里写入本事务的增量；SQLite 提交成功后才替换内存状态。
//!
//! 单实例：连接用 `locking_mode = EXCLUSIVE` 并在打开时立即取得排他锁，同一数据
//! 目录的第二个 `open`（同进程或跨进程）得到 `AlreadyRunning`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gantry_core::canon::Sha256Digest;
use gantry_core::entities::Seq;
use gantry_core::events::Event;
use gantry_core::time::Timestamp;
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::cas::{CasKind, CasStat};
use crate::module::{ModuleRegistry, ModuleTables};
use crate::{Fold, Inner, OutboxRow, StoreError};

/// 本二进制支持的 `meta.schema_version`。
pub const SCHEMA_VERSION: u32 = 2;

const DB_FILE: &str = "gantry.db";

/// 迁移按序、只增（§7.9）；下标 + 1 = 版本号。
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;

CREATE TABLE events (
  seq            INTEGER PRIMARY KEY AUTOINCREMENT,
  id             TEXT    NOT NULL UNIQUE,
  ts             INTEGER NOT NULL,
  type           TEXT    NOT NULL,
  subject        TEXT    NOT NULL,
  project_id     TEXT,
  actor_kind     TEXT    NOT NULL,
  actor_id       TEXT    NOT NULL,
  causation_id   TEXT,
  correlation_id TEXT,
  dedup_key      TEXT UNIQUE,
  data           TEXT    NOT NULL CHECK (json_valid(data))
) STRICT;
CREATE INDEX events_project_seq ON events (project_id, seq);
CREATE INDEX events_type_seq    ON events (type, seq);
CREATE INDEX events_subject_seq ON events (subject, seq);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;

CREATE TABLE consumer_cursors (consumer TEXT PRIMARY KEY, seq INTEGER NOT NULL, updated_at INTEGER NOT NULL) STRICT;

CREATE TABLE outbox (
  id              TEXT PRIMARY KEY,
  idempotency_key TEXT NOT NULL UNIQUE,
  status          TEXT NOT NULL,
  enqueued_seq    INTEGER NOT NULL,
  action          TEXT NOT NULL CHECK (json_valid(action))
) STRICT;
"#,
    r#"
CREATE TABLE cas_objects (
  digest      TEXT PRIMARY KEY,
  bytes       INTEGER NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN ('result_bundle','artifact','progress','quarantine')),
  created_at  INTEGER NOT NULL,
  quarantined INTEGER NOT NULL DEFAULT 0 CHECK (quarantined IN (0, 1))
) STRICT;
"#,
];

/// `Store::open` 的引导配置（store.md §10）。未列出的项取规格默认值。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct StoreConfig {
    /// 数据目录；不存在时创建。
    pub dir: PathBuf,
}

impl StoreConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

pub(crate) struct Durable {
    conn: Mutex<Connection>,
}

impl Durable {
    /// 打开数据目录：取排他锁 → 迁移 → 建模块表。返回持久层与从磁盘载入的状态。
    pub(crate) fn open(
        cfg: &StoreConfig,
        modules: &ModuleRegistry,
        new_log_id: impl FnOnce() -> Result<String, StoreError>,
    ) -> Result<(Durable, Inner), StoreError> {
        create_dir(&cfg.dir)?;
        let mut conn = Connection::open(cfg.dir.join(DB_FILE)).map_err(sqlite)?;
        conn.busy_timeout(std::time::Duration::ZERO)
            .map_err(sqlite)?;
        // EXCLUSIVE 必须在第一次读写前设定；`BEGIN EXCLUSIVE` 立即取锁，被占用即
        // SQLITE_BUSY → AlreadyRunning。
        conn.execute_batch("PRAGMA locking_mode = EXCLUSIVE;")
            .map_err(sqlite)?;
        conn.execute_batch("BEGIN EXCLUSIVE; COMMIT;")
            .map_err(open_error)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA trusted_schema = OFF;",
        )
        .map_err(open_error)?;

        migrate(&mut conn, new_log_id)?;
        for table in modules.tables() {
            // 表名已由 ModuleRegistry 校验为 `<module>_[a-z0-9_]+`，可以安全拼接。
            conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (key TEXT PRIMARY KEY, value TEXT NOT NULL CHECK (json_valid(value))) STRICT;"
            ))
            .map_err(sqlite)?;
        }
        let inner = load(&conn, modules)?;
        Ok((
            Durable {
                conn: Mutex::new(conn),
            },
            inner,
        ))
    }

    /// 在一个 SQLite 事务里写入 `base → next` 的增量。失败时数据库不变。
    pub(crate) fn persist(
        &self,
        base: &Inner,
        next: &Inner,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite)?;

        let dedup_by_seq: BTreeMap<Seq, &String> = next
            .dedup
            .iter()
            .filter(|(_, seq)| seq.as_u64() > base.head)
            .map(|(key, seq)| (*seq, key))
            .collect();
        for event in next.events.iter().skip(base.events.len()) {
            insert_event(&tx, event, dedup_by_seq.get(&event.seq).map(|k| k.as_str()))?;
        }

        for (consumer, seq) in &next.cursors {
            if base.cursors.get(consumer) != Some(seq) {
                upsert_cursor(&tx, consumer, *seq, now)?;
            }
        }

        for (id, row) in &next.outbox {
            if base.outbox.get(id) != Some(row) {
                let action = serde_json::to_string(&row.action).map_err(encode)?;
                let status = serde_json::to_value(row.action.status).map_err(encode)?;
                tx.execute(
                    "INSERT INTO outbox (id, idempotency_key, status, enqueued_seq, action)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (id) DO UPDATE SET status = excluded.status, action = excluded.action",
                    params![
                        id.to_string(),
                        row.action.idempotency_key.to_string(),
                        status.as_str().unwrap_or_default(),
                        row.enqueued_seq.as_u64() as i64,
                        action
                    ],
                )
                .map_err(sqlite)?;
            }
        }

        persist_module_tables(&tx, &base.module_tables, &next.module_tables)?;

        if base.log_epoch != next.log_epoch {
            set_meta(&tx, "log_epoch", &next.log_epoch.to_string())?;
        }
        if base.next_enqueue_seq != next.next_enqueue_seq {
            set_meta(&tx, "next_enqueue_seq", &next.next_enqueue_seq.to_string())?;
        }
        if base.head != next.head {
            set_meta(&tx, "projected_seq", &next.head.to_string())?;
        }
        tx.commit().map_err(sqlite)
    }

    /// CAS 元数据（store.md §7.7）：首次写入定 `kind` 与 `created_at`；`quarantined` 只升不降。
    pub(crate) fn cas_record(
        &self,
        digest: &Sha256Digest,
        stat: &CasStat,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        conn.execute(
            "INSERT INTO cas_objects (digest, bytes, kind, created_at, quarantined)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (digest) DO UPDATE SET quarantined = max(quarantined, excluded.quarantined)",
            params![
                digest.to_hex(),
                stat.bytes as i64,
                stat.kind.as_str(),
                stat.created_at.as_millis(),
                stat.quarantined as i64
            ],
        )
        .map_err(sqlite)?;
        Ok(())
    }

    pub(crate) fn cas_stat(&self, digest: &Sha256Digest) -> Result<Option<CasStat>, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        let row = conn
            .query_row(
                "SELECT bytes, kind, created_at, quarantined FROM cas_objects WHERE digest = ?1",
                params![digest.to_hex()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite)?;
        row.map(|(bytes, kind, created_at, quarantined)| {
            Ok(CasStat {
                bytes: bytes as u64,
                kind: CasKind::parse(&kind)
                    .ok_or_else(|| corrupt(format!("cas_objects.kind {kind}")))?,
                created_at: Timestamp::from_millis(created_at),
                quarantined: quarantined != 0,
            })
        })
        .transpose()
    }

    pub(crate) fn cas_count(&self) -> Result<u64, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        conn.query_row("SELECT count(*) FROM cas_objects", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|n| n as u64)
        .map_err(sqlite)
    }

    /// 直接写的游标（`Cursors::advance`）。
    pub(crate) fn persist_cursor(
        &self,
        consumer: &str,
        seq: Seq,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        upsert_cursor(&conn, consumer, seq, now)
    }

    /// 直接写的 `log_epoch`（恢复后加一，P-67）。`restore_from` 落地前只有 testing 用。
    #[cfg(feature = "testing")]
    pub(crate) fn persist_log_epoch(&self, epoch: u64) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        set_meta(&conn, "log_epoch", &epoch.to_string())
    }

    /// 测试用：直接执行 SQL（验证触发器等数据库层约束）。
    #[cfg(feature = "testing")]
    pub(crate) fn execute_raw(&self, sql: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        conn.execute_batch(sql).map_err(sqlite)
    }
}

fn create_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(|error| StoreError::Io(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    Ok(())
}

fn migrate(
    conn: &mut Connection,
    new_log_id: impl FnOnce() -> Result<String, StoreError>,
) -> Result<(), StoreError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite)?;
    let found = if has_meta {
        get_meta(conn, "schema_version")?
            .map(|text| parse_u64(&text, "schema_version"))
            .transpose()?
            .unwrap_or(0) as u32
    } else {
        0
    };
    if found > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite)?;
    for sql in &MIGRATIONS[found as usize..] {
        tx.execute_batch(sql).map_err(sqlite)?;
    }
    if found == 0 {
        set_meta(&tx, "log_id", &new_log_id()?)?;
        set_meta(&tx, "log_epoch", "1")?;
        set_meta(&tx, "next_enqueue_seq", "1")?;
        set_meta(&tx, "projected_seq", "0")?;
    }
    set_meta(&tx, "schema_version", &SCHEMA_VERSION.to_string())?;
    tx.commit().map_err(sqlite)
}

/// 从磁盘载入：非投影状态直接读取，投影按 `seq` 重放。
fn load(conn: &Connection, modules: &ModuleRegistry) -> Result<Inner, StoreError> {
    let mut inner = Inner {
        log_id: required_meta(conn, "log_id")?,
        log_epoch: parse_u64(&required_meta(conn, "log_epoch")?, "log_epoch")?,
        next_enqueue_seq: parse_u64(
            &required_meta(conn, "next_enqueue_seq")?,
            "next_enqueue_seq",
        )?,
        ..Inner::default()
    };

    // Outbox 是主表：先载入，重放时 `outbox.*` 事件不再改行（`Fold::Replay`）。
    let mut stmt = conn
        .prepare("SELECT action, enqueued_seq FROM outbox")
        .map_err(sqlite)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(sqlite)?;
    for row in rows {
        let (action, enqueued_seq) = row.map_err(sqlite)?;
        let action: gantry_core::entities::OutboxAction =
            serde_json::from_str(&action).map_err(corrupt)?;
        let enqueued_seq = Seq::new(enqueued_seq as u64).map_err(|e| corrupt(e.to_string()))?;
        inner.outbox.insert(
            action.id,
            OutboxRow {
                action,
                enqueued_seq,
            },
        );
    }

    let mut stmt = conn
        .prepare("SELECT consumer, seq FROM consumer_cursors")
        .map_err(sqlite)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(sqlite)?;
    for row in rows {
        let (consumer, seq) = row.map_err(sqlite)?;
        let seq = Seq::new(seq as u64).map_err(|e| corrupt(e.to_string()))?;
        inner.cursors.insert(consumer, seq);
    }

    for table in modules.tables() {
        let mut stmt = conn
            .prepare(&format!("SELECT key, value FROM {table}"))
            .map_err(sqlite)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite)?;
        let entries = inner.module_tables.entry(table).or_default();
        for row in rows {
            let (key, value) = row.map_err(sqlite)?;
            entries.insert(key, serde_json::from_str(&value).map_err(corrupt)?);
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT seq, id, ts, type, subject, project_id, actor_kind, actor_id,
                    causation_id, correlation_id, dedup_key, data
             FROM events ORDER BY seq",
        )
        .map_err(sqlite)?;
    let mut rows = stmt.query([]).map_err(sqlite)?;
    while let Some(row) = rows.next().map_err(sqlite)? {
        let seq: i64 = row.get(0).map_err(sqlite)?;
        let data: String = row.get(11).map_err(sqlite)?;
        let envelope = json!({
            "seq": seq,
            "id": row.get::<_, String>(1).map_err(sqlite)?,
            "ts": Timestamp::from_millis(row.get::<_, i64>(2).map_err(sqlite)?),
            "type": row.get::<_, String>(3).map_err(sqlite)?,
            "subject": row.get::<_, String>(4).map_err(sqlite)?,
            "project_id": row.get::<_, Option<String>>(5).map_err(sqlite)?,
            "actor": {
                "kind": row.get::<_, String>(6).map_err(sqlite)?,
                "id": row.get::<_, String>(7).map_err(sqlite)?,
            },
            "causation_id": row.get::<_, Option<String>>(8).map_err(sqlite)?,
            "correlation_id": row.get::<_, Option<String>>(9).map_err(sqlite)?,
            "data": serde_json::from_str::<Value>(&data).map_err(corrupt)?,
        });
        let event: Event<Value> = serde_json::from_value(envelope).map_err(corrupt)?;
        if event.seq.as_u64() != inner.head + 1 {
            return Err(corrupt(format!(
                "event log has a gap before seq {}",
                event.seq.as_u64()
            )));
        }
        inner
            .project(&event, Fold::Replay)
            .map_err(|error| corrupt(format!("replay of seq {seq} failed: {error}")))?;
        if let Some(key) = row.get::<_, Option<String>>(10).map_err(sqlite)? {
            inner.dedup.insert(key, event.seq);
        }
        inner.head = event.seq.as_u64();
        inner.last_ts = Some(event.ts);
        inner.events.push(event);
    }

    let projected = parse_u64(&required_meta(conn, "projected_seq")?, "projected_seq")?;
    if projected != inner.head {
        return Err(corrupt(format!(
            "projected_seq {projected} does not match head {}",
            inner.head
        )));
    }
    Ok(inner)
}

fn insert_event(
    conn: &Connection,
    event: &Event<Value>,
    dedup_key: Option<&str>,
) -> Result<(), StoreError> {
    let actor = serde_json::to_value(&event.actor).map_err(encode)?;
    let data = serde_json::to_string(&event.data).map_err(encode)?;
    conn.execute(
        "INSERT INTO events (seq, id, ts, type, subject, project_id, actor_kind, actor_id,
                             causation_id, correlation_id, dedup_key, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            event.seq.as_u64() as i64,
            event.id.to_string(),
            event.ts.as_millis(),
            event.ty.as_str(),
            event.subject.as_str(),
            event.project_id.map(|id| id.to_string()),
            actor["kind"].as_str().unwrap_or_default(),
            actor["id"].as_str().unwrap_or_default(),
            event.causation_id.map(|id| id.to_string()),
            event.correlation_id,
            dedup_key,
            data,
        ],
    )
    .map_err(sqlite)?;
    Ok(())
}

fn upsert_cursor(
    conn: &Connection,
    consumer: &str,
    seq: Seq,
    now: Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO consumer_cursors (consumer, seq, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (consumer) DO UPDATE SET seq = excluded.seq, updated_at = excluded.updated_at",
        params![consumer, seq.as_u64() as i64, now.as_millis()],
    )
    .map_err(sqlite)?;
    Ok(())
}

fn persist_module_tables(
    conn: &Connection,
    base: &ModuleTables,
    next: &ModuleTables,
) -> Result<(), StoreError> {
    let empty = BTreeMap::new();
    for (table, rows) in next {
        let before = base.get(table).unwrap_or(&empty);
        for (key, value) in rows {
            if before.get(key) != Some(value) {
                conn.execute(
                    &format!(
                        "INSERT INTO {table} (key, value) VALUES (?1, ?2)
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value"
                    ),
                    params![key, serde_json::to_string(value).map_err(encode)?],
                )
                .map_err(sqlite)?;
            }
        }
        for key in before.keys().filter(|key| !rows.contains_key(*key)) {
            conn.execute(&format!("DELETE FROM {table} WHERE key = ?1"), params![key])
                .map_err(sqlite)?;
        }
    }
    for (table, rows) in base {
        if !next.contains_key(table) {
            for key in rows.keys() {
                conn.execute(&format!("DELETE FROM {table} WHERE key = ?1"), params![key])
                    .map_err(sqlite)?;
            }
        }
    }
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite)
}

fn required_meta(conn: &Connection, key: &str) -> Result<String, StoreError> {
    get_meta(conn, key)?.ok_or_else(|| corrupt(format!("meta.{key} is missing")))
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(sqlite)?;
    Ok(())
}

fn parse_u64(text: &str, key: &str) -> Result<u64, StoreError> {
    text.parse()
        .map_err(|_| corrupt(format!("meta.{key} is not a number: {text}")))
}

fn sqlite(error: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        match failure.code {
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                return StoreError::Corrupt(error.to_string())
            }
            ErrorCode::DiskFull => return StoreError::DiskFull,
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => return StoreError::Busy,
            _ => {}
        }
    }
    StoreError::Sqlite(error.to_string())
}

/// 打开阶段的锁冲突说明另一个实例持有数据目录。
fn open_error(error: rusqlite::Error) -> StoreError {
    match sqlite(error) {
        StoreError::Busy => StoreError::AlreadyRunning,
        other => other,
    }
}

fn encode(error: serde_json::Error) -> StoreError {
    StoreError::Sqlite(format!("encode: {error}"))
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}
