//! Durable SQLite store for the write queue, observed diffs, and backend
//! readiness state.
//!
//! Schema is straight from the brief. WAL mode lets the proxy keep reading
//! while the write-replay worker enqueues new rows.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection};

use crate::DbPool;

pub fn open(path: &Path) -> Result<DbPool> {
    let conn = Connection::open(path)?;
    // WAL: readers don't block the single writer, and crash-recovery is much
    // better than rollback-journal. busy_timeout(5s): if the write-replay
    // worker is mid-transaction, request handlers wait up to 5s before they
    // give up — well above our p99 target.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000_i64)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(Arc::new(Mutex::new(conn)))
}

pub fn migrate(pool: &DbPool) -> Result<()> {
    let conn = pool.lock();
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS write_queue (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            enqueued_at_ns  INTEGER NOT NULL,
            method          TEXT    NOT NULL,
            path            TEXT    NOT NULL,
            body            BLOB,
            content_type    TEXT,
            attempts        INTEGER NOT NULL DEFAULT 0,
            last_error      TEXT,
            shadow_status   INTEGER,
            next_attempt_at_ns INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS write_queue_next_idx
            ON write_queue(next_attempt_at_ns);

        CREATE TABLE IF NOT EXISTS diffs (
            id                            INTEGER PRIMARY KEY AUTOINCREMENT,
            observed_at_ns                INTEGER NOT NULL,
            method                        TEXT    NOT NULL,
            path                          TEXT    NOT NULL,
            primary_status                INTEGER,
            shadow_status                 INTEGER,
            primary_body                  TEXT,
            shadow_body                   TEXT,
            diff_blob                     TEXT,
            severity                      TEXT NOT NULL,
            descended_from_failed_write_id INTEGER
        );

        CREATE INDEX IF NOT EXISTS diffs_observed_idx ON diffs(observed_at_ns DESC);

        CREATE TABLE IF NOT EXISTS failed_writes (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            original_queue_id INTEGER,
            enqueued_at_ns  INTEGER NOT NULL,
            failed_at_ns    INTEGER NOT NULL,
            method          TEXT NOT NULL,
            path            TEXT NOT NULL,
            body            BLOB,
            content_type    TEXT,
            attempts        INTEGER NOT NULL,
            last_error      TEXT
        );

        CREATE TABLE IF NOT EXISTS backend_state (
            backend                       TEXT PRIMARY KEY,
            last_observed_uptime_seconds  INTEGER,
            last_observed_git_sha         TEXT,
            state                         TEXT NOT NULL DEFAULT 'live',
            updated_at_ns                 INTEGER NOT NULL DEFAULT 0
        );

        INSERT OR IGNORE INTO backend_state(backend, state, updated_at_ns)
            VALUES ('primary', 'seeded', 0), ('shadow', 'seeded', 0);
        "#,
    )?;
    Ok(())
}

pub fn enqueue_write(
    pool: &DbPool,
    method: &str,
    path: &str,
    body: &[u8],
    content_type: Option<&str>,
) -> Result<i64> {
    let now = now_ns();
    let conn = pool.lock();
    conn.execute(
        "INSERT INTO write_queue (enqueued_at_ns, method, path, body, content_type, next_attempt_at_ns)
            VALUES (?1, ?2, ?3, ?4, ?5, ?1)",
        params![now, method, path, body, content_type],
    )?;
    Ok(conn.last_insert_rowid())
}

pub struct QueuedWrite {
    pub id: i64,
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub attempts: u32,
}

/// Pop one write whose `next_attempt_at_ns <= now` (FIFO within ready set).
/// Caller is expected to call `mark_succeeded` or `mark_failed_attempt`.
pub fn pop_one_ready(pool: &DbPool) -> Result<Option<QueuedWrite>> {
    let now = now_ns();
    let conn = pool.lock();
    let mut stmt = conn.prepare(
        "SELECT id, method, path, body, content_type, attempts
           FROM write_queue
          WHERE next_attempt_at_ns <= ?1
          ORDER BY id ASC
          LIMIT 1",
    )?;
    let row = stmt
        .query_row(params![now], |r| {
            Ok(QueuedWrite {
                id: r.get(0)?,
                method: r.get(1)?,
                path: r.get(2)?,
                body: r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                content_type: r.get(4)?,
                attempts: r.get::<_, i64>(5)? as u32,
            })
        })
        .ok();
    Ok(row)
}

pub fn mark_succeeded(pool: &DbPool, id: i64, status: u16) -> Result<()> {
    let conn = pool.lock();
    conn.execute(
        "UPDATE write_queue SET shadow_status = ?2 WHERE id = ?1",
        params![id, status as i64],
    )?;
    conn.execute("DELETE FROM write_queue WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn mark_failed_attempt(pool: &DbPool, id: i64, err: &str, backoff_ms: u64) -> Result<u32> {
    let next = now_ns() + (backoff_ms as i64 * 1_000_000);
    let conn = pool.lock();
    conn.execute(
        "UPDATE write_queue
            SET attempts = attempts + 1,
                last_error = ?2,
                next_attempt_at_ns = ?3
          WHERE id = ?1",
        params![id, err, next],
    )?;
    let attempts: i64 = conn
        .query_row(
            "SELECT attempts FROM write_queue WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(attempts as u32)
}

/// Move a queue row to failed_writes and delete it from the queue.
pub fn move_to_failed(pool: &DbPool, id: i64) -> Result<i64> {
    let now = now_ns();
    let conn = pool.lock();
    conn.execute(
        "INSERT INTO failed_writes
            (original_queue_id, enqueued_at_ns, failed_at_ns, method, path, body, content_type, attempts, last_error)
         SELECT id, enqueued_at_ns, ?2, method, path, body, content_type, attempts, last_error
           FROM write_queue WHERE id = ?1",
        params![id, now],
    )?;
    let failed_id = conn.last_insert_rowid();
    conn.execute("DELETE FROM write_queue WHERE id = ?1", params![id])?;
    Ok(failed_id)
}

pub fn queue_depth(pool: &DbPool) -> Result<i64> {
    let conn = pool.lock();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM write_queue", [], |r| r.get(0))?;
    Ok(n)
}

pub fn failed_count(pool: &DbPool) -> Result<i64> {
    let conn = pool.lock();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM failed_writes", [], |r| r.get(0))?;
    Ok(n)
}

#[allow(clippy::too_many_arguments)]
pub fn record_diff(
    pool: &DbPool,
    method: &str,
    path: &str,
    primary_status: Option<u16>,
    shadow_status: Option<u16>,
    primary_body: Option<&str>,
    shadow_body: Option<&str>,
    diff_blob: &str,
    severity: &str,
    descended_from: Option<i64>,
) -> Result<i64> {
    let now = now_ns();
    let conn = pool.lock();
    conn.execute(
        "INSERT INTO diffs (observed_at_ns, method, path, primary_status, shadow_status,
            primary_body, shadow_body, diff_blob, severity, descended_from_failed_write_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            now,
            method,
            path,
            primary_status.map(|s| s as i64),
            shadow_status.map(|s| s as i64),
            primary_body,
            shadow_body,
            diff_blob,
            severity,
            descended_from
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

#[derive(serde::Serialize)]
pub struct DiffRow {
    pub id: i64,
    pub observed_at_ns: i64,
    pub method: String,
    pub path: String,
    pub primary_status: Option<i64>,
    pub shadow_status: Option<i64>,
    pub diff_blob: String,
    pub severity: String,
    pub descended_from_failed_write_id: Option<i64>,
}

pub fn list_recent_diffs(pool: &DbPool, limit: i64) -> Result<Vec<DiffRow>> {
    list_diffs_filtered(pool, limit, None, None)
}

/// List diffs ordered by `observed_at_ns DESC`, with optional filters used by
/// the dashboard:
///
/// * `severity_eq` — exact severity match (`critical|high|medium|low|noise`).
/// * `since_ns`    — only return rows with `observed_at_ns >= since_ns`.
pub fn list_diffs_filtered(
    pool: &DbPool,
    limit: i64,
    severity_eq: Option<&str>,
    since_ns: Option<i64>,
) -> Result<Vec<DiffRow>> {
    let conn = pool.lock();
    // Build SQL dynamically — only two well-known shapes; bind values with
    // params to keep this injection-safe.
    let mut sql = String::from(
        "SELECT id, observed_at_ns, method, path, primary_status, shadow_status,
                diff_blob, severity, descended_from_failed_write_id
           FROM diffs WHERE 1=1",
    );
    if severity_eq.is_some() {
        sql.push_str(" AND severity = ?");
    }
    if since_ns.is_some() {
        sql.push_str(" AND observed_at_ns >= ?");
    }
    sql.push_str(" ORDER BY observed_at_ns DESC LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = severity_eq {
        bindings.push(rusqlite::types::Value::Text(s.to_string()));
    }
    if let Some(ns) = since_ns {
        bindings.push(rusqlite::types::Value::Integer(ns));
    }
    bindings.push(rusqlite::types::Value::Integer(limit));

    let rows = stmt
        .query_map(rusqlite::params_from_iter(bindings.iter()), |r| {
            Ok(DiffRow {
                id: r.get(0)?,
                observed_at_ns: r.get(1)?,
                method: r.get(2)?,
                path: r.get(3)?,
                primary_status: r.get(4)?,
                shadow_status: r.get(5)?,
                diff_blob: r.get(6)?,
                severity: r.get(7)?,
                descended_from_failed_write_id: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Full single-diff record (including bodies) for the drill-in page.
#[derive(serde::Serialize)]
pub struct DiffDetail {
    pub id: i64,
    pub observed_at_ns: i64,
    pub method: String,
    pub path: String,
    pub primary_status: Option<i64>,
    pub shadow_status: Option<i64>,
    pub primary_body: Option<String>,
    pub shadow_body: Option<String>,
    pub diff_blob: String,
    pub severity: String,
    pub descended_from_failed_write_id: Option<i64>,
}

pub fn get_diff(pool: &DbPool, id: i64) -> Result<Option<DiffDetail>> {
    let conn = pool.lock();
    let row = conn
        .query_row(
            "SELECT id, observed_at_ns, method, path, primary_status, shadow_status,
                    primary_body, shadow_body, diff_blob, severity,
                    descended_from_failed_write_id
               FROM diffs WHERE id = ?1",
            params![id],
            |r| {
                Ok(DiffDetail {
                    id: r.get(0)?,
                    observed_at_ns: r.get(1)?,
                    method: r.get(2)?,
                    path: r.get(3)?,
                    primary_status: r.get(4)?,
                    shadow_status: r.get(5)?,
                    primary_body: r.get(6)?,
                    shadow_body: r.get(7)?,
                    diff_blob: r.get(8)?,
                    severity: r.get(9)?,
                    descended_from_failed_write_id: r.get(10)?,
                })
            },
        )
        .ok();
    Ok(row)
}

#[derive(Clone, Debug)]
pub struct BackendStateRow {
    pub backend: String,
    pub last_observed_uptime_seconds: Option<i64>,
    pub last_observed_git_sha: Option<String>,
    pub state: String,
}

pub fn get_backend_state(pool: &DbPool, backend: &str) -> Result<Option<BackendStateRow>> {
    let conn = pool.lock();
    let row = conn
        .query_row(
            "SELECT backend, last_observed_uptime_seconds, last_observed_git_sha, state
               FROM backend_state WHERE backend = ?1",
            params![backend],
            |r| {
                Ok(BackendStateRow {
                    backend: r.get(0)?,
                    last_observed_uptime_seconds: r.get(1)?,
                    last_observed_git_sha: r.get(2)?,
                    state: r.get(3)?,
                })
            },
        )
        .ok();
    Ok(row)
}

pub fn upsert_backend_state(
    pool: &DbPool,
    backend: &str,
    uptime: i64,
    git_sha: &str,
    state: &str,
) -> Result<()> {
    let now = now_ns();
    let conn = pool.lock();
    conn.execute(
        "INSERT INTO backend_state(backend, last_observed_uptime_seconds, last_observed_git_sha, state, updated_at_ns)
            VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(backend) DO UPDATE SET
            last_observed_uptime_seconds = excluded.last_observed_uptime_seconds,
            last_observed_git_sha        = excluded.last_observed_git_sha,
            state                        = excluded.state,
            updated_at_ns                = excluded.updated_at_ns",
        params![backend, uptime, git_sha, state, now],
    )?;
    Ok(())
}

pub fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
