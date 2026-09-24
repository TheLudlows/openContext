//! SQLite relational store, queue and lifecycle (M2).
//!
//! The local relational backend: one SQLite database file holds the
//! authoritative tenant/workspace objects, the idempotency ledger, audit, job
//! registration and the self-polled job queue (A5). Vector and graph data live
//! in other backends (M4); raw file bytes live on the local filesystem.
//!
//! Encodings are fixed in [`sqlite-schema.sql`](include_str!("sqlite-schema.sql")).

// Domain operations map 1:1 onto A5 table rows: wide row tuples and wide
// signatures are inherent to the adapter boundary and are clearer inline than
// factored into a per-query struct.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use sqlx::Transaction;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use uuid::Uuid;

use crate::storage::Lifecycle;
use crate::storage::error::{StorageError, StorageResult};
use crate::storage::ledger::{
    LEDGER_PENDING_TIMEOUT_MS, LedgerEntry, LedgerKey, MAX_LEDGER_ATTEMPTS,
};
use crate::storage::scope::{AuthorizedScope, Permission, Scope, SourceVersion};
use crate::storage::traits::{
    ClaimedJob, DomainTx, IssuedKey, JobFinish, JobQueue, RelationalStore, WorkItem,
};

/// The complete schema for a new database, executed only when absent (A2.3).
const SCHEMA: &str = include_str!("sqlite-schema.sql");

/// Tables every compatible database must contain (A5).
const REQUIRED_TABLES: &[&str] = &[
    "oc_workspaces",
    "oc_api_keys",
    "oc_files",
    "oc_events",
    "oc_assets",
    "oc_versions",
    "oc_candidates",
    "oc_reviews",
    "oc_jobs",
    "oc_chunks",
    "oc_summaries",
    "oc_commands",
    "oc_audit",
    "oc_artifact_owners",
    "oc_index_entries",
    "oc_artifact_ledger",
];

/// Columns the self-polled queue depends on; a missing one means the structure
/// is incompatible, not merely outdated (A2.5).
const REQUIRED_JOB_COLUMNS: &[&str] = &[
    "state",
    "generation",
    "run_token",
    "cancel_requested",
    "attempt",
    "next_retry_at",
];

/// The local relational store backed by a single SQLite database file.
pub struct SqliteStore {
    pool: SqlitePool,
    path: PathBuf,
    /// In-process lock holder bound to the database path (A2.5). Dropping it
    /// (or crashing the process) releases the OS lock.
    _lock: LockGuard,
}

impl SqliteStore {
    /// Open the store at `path`, creating the schema when the file is new.
    pub async fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Take the single-writer lock before touching the database: a second
        // process that opens the same path fails fast here instead of racing
        // the running worker (A2.5). Reentrant within this process.
        let lock = acquire_lock(&path)?;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(sqlite_err)?;
        let store = Self {
            pool,
            path,
            _lock: lock,
        };
        store.initialize().await?;
        Ok(store)
    }

    /// The shared connection pool. Domain operations borrow short-lived
    /// connections; model/file IO never holds one (A2.5).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock file path bound to a database path (A2.5).
fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// One in-process holder of the OS lock and its open-store count. The OS lock
/// lives on the single `file` handle; in-process reopens bump `refs` without
/// re-taking the OS lock, so the API and Worker (one process) share one holder
/// while a second process is still rejected (A2.5).
struct HeldLock {
    /// Owned solely to hold the OS lock until the last in-process store drops.
    #[allow(dead_code)]
    file: File,
    refs: usize,
}

/// Locks held by this process, keyed by lock-file path.
static PROCESS_LOCKS: LazyLock<Mutex<HashMap<PathBuf, HeldLock>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Take the single-writer lock for `path`, returning a guard that releases it
/// on drop. Reentrant within this process (A2.5).
fn acquire_lock(path: &Path) -> StorageResult<LockGuard> {
    let key = lock_path(path);
    let mut locks = PROCESS_LOCKS
        .lock()
        .expect("process lock registry poisoned");
    if let Some(held) = locks.get_mut(&key) {
        held.refs += 1;
        return Ok(LockGuard(key));
    }
    let file = File::create(&key)?;
    file.try_lock_exclusive().map_err(|e| {
        StorageError::Conflict(format!("store lock is held by another process: {e}"))
    })?;
    locks.insert(key.clone(), HeldLock { file, refs: 1 });
    Ok(LockGuard(key))
}

/// Release one in-process reference; the last one drops the OS-locked file.
fn release_lock(key: &Path) {
    let mut locks = PROCESS_LOCKS
        .lock()
        .expect("process lock registry poisoned");
    if let Some(held) = locks.get_mut(key) {
        held.refs -= 1;
        if held.refs == 0 {
            locks.remove(key);
        }
    }
}

/// Guard that releases its in-process lock reference on drop.
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        release_lock(&self.0);
    }
}

fn sqlite_err(e: sqlx::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

async fn schema_present(conn: &mut SqliteConnection) -> StorageResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='oc_workspaces'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlite_err)?;
    Ok(n > 0)
}

async fn exec_schema(conn: &mut SqliteConnection) -> StorageResult<()> {
    for stmt in SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::raw_sql(stmt)
            .execute(&mut *conn)
            .await
            .map_err(sqlite_err)?;
    }
    Ok(())
}

async fn check_conn(conn: &mut SqliteConnection) -> StorageResult<()> {
    for table in REQUIRED_TABLES {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        )
        .bind(table)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlite_err)?;
        if !present {
            return Err(StorageError::Unavailable(format!(
                "incompatible SQLite schema: missing table {table}"
            )));
        }
    }
    for column in REQUIRED_JOB_COLUMNS {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('oc_jobs') WHERE name=?)",
        )
        .bind(column)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlite_err)?;
        if !present {
            return Err(StorageError::Unavailable(format!(
                "incompatible SQLite schema: missing column oc_jobs.{column}"
            )));
        }
    }
    Ok(())
}

impl Lifecycle for SqliteStore {
    async fn initialize(&self) -> StorageResult<()> {
        let mut conn = self.pool.acquire().await.map_err(sqlite_err)?;
        // BEGIN IMMEDIATE serializes concurrent initializers: a second one
        // waits on busy_timeout, then observes the now-present schema instead
        // of racing the first (A2.3).
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(sqlite_err)?;
        let outcome = Self::init_inner(&mut conn).await;
        match outcome {
            Ok(()) => {
                sqlx::query("COMMIT")
                    .execute(&mut *conn)
                    .await
                    .map_err(sqlite_err)?;
                Ok(())
            }
            Err(e) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(e)
            }
        }
    }

    async fn check(&self) -> StorageResult<()> {
        let mut conn = self.pool.acquire().await.map_err(sqlite_err)?;
        check_conn(&mut conn).await
    }

    async fn shutdown(&self) -> StorageResult<()> {
        self.pool.close().await;
        Ok(())
    }
}

impl SqliteStore {
    async fn init_inner(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) -> StorageResult<()> {
        if !schema_present(&mut *conn).await? {
            exec_schema(&mut *conn).await?;
        }
        check_conn(&mut *conn).await
    }
}

// ---------------------------------------------------------------------------
// Privileged operations, scoped transactions and the self-polled queue
// ---------------------------------------------------------------------------

/// Roles an API key may hold (A2.2 role matrix).
const ROLES: &[&str] = &["reader", "writer", "reviewer", "admin"];

/// A job left in `processing` longer than this is treated as abandoned by a
/// crashed worker and requeued by the next claim (A2.5).
const PROCESSING_TIMEOUT_MS: i64 = 5 * 60 * 1000;

/// SHA-256 hex digest, shared with the PostgreSQL adapter's token hash.
fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Exponential retry delay in milliseconds, clamped (A2.5).
fn retry_delay(attempt: i32) -> i64 {
    (1i64 << attempt.clamp(0, 20) as u32) * 1000
}

/// The idempotency key opened by [`DomainTx::begin_command`] and closed by
/// [`DomainTx::finish_command`].
struct PendingCommand {
    operation: String,
    key: String,
    hash: String,
}

/// Build the API-facing candidate JSON (candidate columns + `current_version`).
fn candidate_json(
    (
        id,
        asset_id,
        source_event_id,
        fact_key,
        content,
        revision,
        expected_version,
        state,
        created_at,
        current_version,
    ): (
        Uuid,
        Option<Uuid>,
        Uuid,
        String,
        String,
        i32,
        Option<i32>,
        String,
        i64,
        Option<i32>,
    ),
) -> Value {
    json!({
        "id": id,
        "asset_id": asset_id,
        "source_event_id": source_event_id,
        "fact_key": fact_key,
        "content": content,
        "revision": revision,
        "expected_version": expected_version,
        "state": state,
        "created_at": created_at,
        "current_version": current_version,
    })
}

/// A scoped transaction over one SQLite connection. Writes execute inside the
/// underlying transaction, so `commit`/`rollback` make them atomic (A2.2).
pub struct SqliteTx {
    tx: Transaction<'static, sqlx::Sqlite>,
    pool: SqlitePool,
    scope: Scope,
    principal_id: Uuid,
    required: Vec<Permission>,
    command: Option<PendingCommand>,
}

impl SqliteTx {
    /// The principal's role as of right now, read on a fresh connection so a
    /// revoke committed mid-transaction is observed (A2.2 commit re-check).
    async fn live_role(&self) -> StorageResult<String> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM oc_api_keys WHERE id = ? AND NOT revoked")
                .bind(self.principal_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(sqlite_err)?;
        role.ok_or(StorageError::Forbidden)
    }
}

impl DomainTx for SqliteTx {
    fn scope(&self) -> Scope {
        self.scope
    }

    async fn check_permission(&mut self, permission: Permission) -> StorageResult<()> {
        let role = self.live_role().await?;
        if permission.granted_by(&role) {
            self.required.push(permission);
            Ok(())
        } else {
            Err(StorageError::Forbidden)
        }
    }

    async fn begin_command(
        &mut self,
        operation: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> StorageResult<Option<Value>> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT request_hash, response FROM oc_commands \
             WHERE tenant_id = ? AND workspace_id = ? AND principal_id = ? AND operation = ? AND key = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(self.principal_id)
        .bind(operation)
        .bind(idempotency_key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        match row {
            Some((hash, response)) if hash == request_hash => {
                Ok(Some(serde_json::from_str(&response).unwrap_or(Value::Null)))
            }
            Some((_hash, _response)) => Err(StorageError::Conflict(format!(
                "idempotency key {idempotency_key} reused with different input"
            ))),
            None => {
                self.command = Some(PendingCommand {
                    operation: operation.to_string(),
                    key: idempotency_key.to_string(),
                    hash: request_hash.to_string(),
                });
                Ok(None)
            }
        }
    }

    async fn record_source(&mut self, _source: SourceVersion) -> StorageResult<()> {
        // The domain operations that materialise a source (event/version) land
        // with the full adapter; the envelope itself records nothing.
        Ok(())
    }

    async fn enqueue(&mut self, work: WorkItem) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_jobs(tenant_id, workspace_id, id, created_by, operation, payload, \
             state, run_token, generation, attempt, asset_id, source_event_id, cancel_requested, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'pending', 0, 1, 0, ?, ?, 0, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(work.job_id)
        .bind(self.principal_id)
        .bind(work.kind)
        .bind(serde_json::to_string(&work.payload).unwrap_or_default())
        .bind(work.asset)
        .bind(work.source)
        .bind(now_ms())
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn audit(&mut self, action: &str, target: Uuid, detail: Value) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_audit(tenant_id, workspace_id, id, actor, action, target, details, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(Uuid::new_v4())
        .bind(self.principal_id)
        .bind(action)
        .bind(target)
        .bind(serde_json::to_string(&detail).unwrap_or_default())
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn finish_command(&mut self, response: Value) -> StorageResult<()> {
        let command = self.command.take().ok_or_else(|| {
            StorageError::Unavailable("finish_command without begin_command".to_string())
        })?;
        sqlx::query(
            "INSERT INTO oc_commands(tenant_id, workspace_id, principal_id, operation, key, request_hash, response, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(self.principal_id)
        .bind(command.operation)
        .bind(command.key)
        .bind(command.hash)
        .bind(serde_json::to_string(&response).unwrap_or_default())
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn create_event(
        &mut self,
        kind: &str,
        content: &str,
        file: Option<Uuid>,
    ) -> StorageResult<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oc_events(tenant_id, workspace_id, id, content, kind, file_id, state, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .bind(content)
        .bind(kind)
        .bind(file)
        .bind(self.principal_id)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(id)
    }

    async fn slot(&mut self, fact_key: &str) -> StorageResult<(Uuid, Option<i32>)> {
        let existing: Option<(Uuid, Option<i32>, bool)> = sqlx::query_as(
            "SELECT id, current_version, deleted FROM oc_assets \
             WHERE tenant_id = ? AND workspace_id = ? AND kind = 'memory' AND fact_key = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(fact_key)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        if let Some((id, version, deleted)) = existing {
            if deleted {
                return Err(StorageError::Conflict(
                    "fact slot was deleted; use a new fact key".to_string(),
                ));
            }
            return Ok((id, version));
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oc_assets(tenant_id, workspace_id, id, kind, title, fact_key, created_at) \
             VALUES (?, ?, ?, 'memory', ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .bind(fact_key)
        .bind(fact_key)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok((id, None))
    }

    async fn insert_candidate(
        &mut self,
        asset: Uuid,
        fact_key: &str,
        content: &str,
        source: Uuid,
        approved: bool,
        expected_version: Option<i32>,
    ) -> StorageResult<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oc_candidates(tenant_id, workspace_id, id, asset_id, source_event_id, fact_key, content, revision, expected_version, state, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .bind(asset)
        .bind(source)
        .bind(fact_key)
        .bind(content)
        .bind(expected_version)
        .bind(if approved { "approved" } else { "candidate" })
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(id)
    }

    async fn insert_knowledge_asset(&mut self, title: &str) -> StorageResult<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oc_assets(tenant_id, workspace_id, id, kind, title, created_at) \
             VALUES (?, ?, ?, 'knowledge', ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .bind(title)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(id)
    }

    async fn insert_file(
        &mut self,
        name: &str,
        media_type: &str,
        hash: &str,
        size: i64,
    ) -> StorageResult<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oc_files(tenant_id, workspace_id, id, name, media_type, hash, size, deleted, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .bind(name)
        .bind(media_type)
        .bind(hash)
        .bind(size)
        .bind(self.principal_id)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(id)
    }

    async fn file_exists(&mut self, id: Uuid) -> StorageResult<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM oc_files WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND NOT deleted)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(sqlite_err)
    }

    async fn file_hash(&mut self, id: Uuid) -> StorageResult<Option<String>> {
        sqlx::query_scalar(
            "SELECT hash FROM oc_files WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND NOT deleted",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)
    }

    async fn file_visible(&mut self, id: Uuid) -> StorageResult<bool> {
        self.file_exists(id).await
    }

    async fn candidate_for_review(&mut self, id: Uuid) -> StorageResult<Value> {
        let row: Option<(
            Uuid,
            Uuid,
            Uuid,
            String,
            String,
            i32,
            Option<i32>,
            String,
            Option<i32>,
            bool,
            String,
        )> = sqlx::query_as(
            "SELECT c.id, c.asset_id, c.source_event_id, c.fact_key, c.content, c.revision, \
                    c.expected_version, c.state, a.current_version, a.deleted, e.state \
             FROM oc_candidates c \
             JOIN oc_assets a ON a.id = c.asset_id AND a.tenant_id = c.tenant_id AND a.workspace_id = c.workspace_id \
             JOIN oc_events e ON e.id = c.source_event_id AND e.tenant_id = c.tenant_id AND e.workspace_id = c.workspace_id \
             WHERE c.id = ? AND c.tenant_id = ? AND c.workspace_id = ?",
        )
        .bind(id)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((
            candidate_id,
            asset_id,
            source_event_id,
            fact_key,
            content,
            revision,
            expected_version,
            state,
            current_version,
            deleted,
            source_state,
        )) = row
        else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "id": candidate_id,
            "asset_id": asset_id,
            "source_event_id": source_event_id,
            "fact_key": fact_key,
            "content": content,
            "revision": revision,
            "expected_version": expected_version,
            "state": state,
            "current_version": current_version,
            "deleted": deleted,
            "source_state": source_state,
        }))
    }

    async fn apply_review(
        &mut self,
        review: Uuid,
        candidate: Uuid,
        revision: i32,
        decision: &str,
        expected_version: Option<i32>,
        reason: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_reviews(tenant_id, workspace_id, id, candidate_id, revision, decision, expected_version, reviewer, reason, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(review)
        .bind(candidate)
        .bind(revision)
        .bind(decision)
        .bind(expected_version)
        .bind(self.principal_id)
        .bind(reason)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        sqlx::query(
            "UPDATE oc_candidates SET state = ?, expected_version = ? WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(if decision == "approve" { "approved" } else { "rejected" })
        .bind(expected_version)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(candidate)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn restore_source(&mut self, asset: Uuid, version: i32) -> StorageResult<Value> {
        let row: Option<(Option<i32>, Uuid, String)> = sqlx::query_as(
            "SELECT a.current_version, v.source_event_id, v.title \
             FROM oc_assets a \
             JOIN oc_versions v ON v.asset_id = a.id AND v.tenant_id = a.tenant_id AND v.workspace_id = a.workspace_id \
             JOIN oc_events e ON e.id = v.source_event_id AND e.tenant_id = v.tenant_id AND e.workspace_id = v.workspace_id \
             WHERE a.id = ? AND a.tenant_id = ? AND a.workspace_id = ? AND v.version = ? AND NOT a.deleted AND e.state = 'active'",
        )
        .bind(asset)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(version)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((current_version, source_event_id, title)) = row else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "current_version": current_version,
            "source_event_id": source_event_id,
            "title": title,
        }))
    }

    async fn delete_asset(&mut self, id: Uuid) -> StorageResult<u64> {
        sqlx::query(
            "UPDATE oc_assets SET deleted = 1, current_version = NULL WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)
        .map(|r| r.rows_affected())
    }

    async fn retract_event(&mut self, id: Uuid) -> StorageResult<u64> {
        sqlx::query(
            "UPDATE oc_events SET state = 'retracted' WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)
        .map(|r| r.rows_affected())
    }

    async fn delete_file(&mut self, id: Uuid) -> StorageResult<u64> {
        let count = sqlx::query(
            "UPDATE oc_files SET deleted = 1 WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?
        .rows_affected();
        sqlx::query(
            "UPDATE oc_events SET state = 'retracted' WHERE tenant_id = ? AND workspace_id = ? AND file_id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(count)
    }

    async fn withdraw_affected_candidates(&mut self) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_candidates SET state = 'withdrawn', revision = revision + 1 \
             WHERE state IN ('candidate', 'approved') \
               AND (EXISTS(SELECT 1 FROM oc_assets a WHERE a.id = oc_candidates.asset_id AND a.tenant_id = oc_candidates.tenant_id AND a.workspace_id = oc_candidates.workspace_id AND a.deleted) \
                 OR EXISTS(SELECT 1 FROM oc_events e WHERE e.id = oc_candidates.source_event_id AND e.tenant_id = oc_candidates.tenant_id AND e.workspace_id = oc_candidates.workspace_id AND e.state = 'retracted'))",
        )
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn cancel_affected_jobs(&mut self) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_jobs SET state = 'cancelled', cancel_requested = 1, run_token = run_token + 1, updated_at = ? \
             WHERE operation <> 'cleanup' AND state IN ('pending', 'processing', 'failed', 'retry_wait') \
               AND (EXISTS(SELECT 1 FROM oc_assets a WHERE a.id = oc_jobs.asset_id AND a.tenant_id = oc_jobs.tenant_id AND a.workspace_id = oc_jobs.workspace_id AND a.deleted) \
                 OR EXISTS(SELECT 1 FROM oc_events e WHERE e.id = oc_jobs.source_event_id AND e.tenant_id = oc_jobs.tenant_id AND e.workspace_id = oc_jobs.workspace_id AND e.state = 'retracted'))",
        )
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn job_row(&mut self, id: Uuid) -> StorageResult<Value> {
        let row: Option<(Uuid, String, String, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT created_by, state, operation, asset_id, source_event_id FROM oc_jobs \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((created_by, state, operation, asset, source)) = row else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "created_by": created_by,
            "state": state,
            "operation": operation,
            "asset_id": asset,
            "source_event_id": source,
        }))
    }

    async fn set_job_action(&mut self, id: Uuid, action: &str) -> StorageResult<i64> {
        sqlx::query_scalar(
            "UPDATE oc_jobs SET state = ?, generation = generation + 1, run_token = run_token + 1, \
                    cancel_requested = ?, error_code = NULL, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ? RETURNING generation",
        )
        .bind(if action == "cancel" {
            "cancelled"
        } else {
            "pending"
        })
        .bind(action == "cancel")
        .bind(now_ms())
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(sqlite_err)
    }

    async fn valid_targets(
        &mut self,
        asset: Option<Uuid>,
        source: Option<Uuid>,
    ) -> StorageResult<()> {
        if let Some(id) = asset {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oc_assets WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND NOT deleted)",
            )
            .bind(self.scope.tenant_id)
            .bind(self.scope.workspace_id)
            .bind(id)
            .fetch_one(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
            if !valid {
                return Err(StorageError::Conflict(
                    "asset is deleted or missing".to_string(),
                ));
            }
        }
        if let Some(id) = source {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oc_events WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND state = 'active')",
            )
            .bind(self.scope.tenant_id)
            .bind(self.scope.workspace_id)
            .bind(id)
            .fetch_one(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
            if !valid {
                return Err(StorageError::Conflict(
                    "source is retracted or missing".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn candidate_view(&mut self, id: Uuid) -> StorageResult<Value> {
        let row: Option<(
            Uuid,
            Option<Uuid>,
            Uuid,
            String,
            String,
            i32,
            Option<i32>,
            String,
            i64,
            Option<i32>,
        )> = sqlx::query_as(
            "SELECT c.id, c.asset_id, c.source_event_id, c.fact_key, c.content, c.revision, \
                    c.expected_version, c.state, c.created_at, a.current_version \
             FROM oc_candidates c \
             JOIN oc_assets a ON a.id = c.asset_id AND a.tenant_id = c.tenant_id AND a.workspace_id = c.workspace_id \
             JOIN oc_events e ON e.id = c.source_event_id AND e.tenant_id = c.tenant_id AND e.workspace_id = c.workspace_id \
             WHERE c.id = ? AND c.tenant_id = ? AND c.workspace_id = ? AND NOT a.deleted AND e.state = 'active'",
        )
        .bind(id)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some(row) = row else {
            return Err(StorageError::NotFound);
        };
        Ok(candidate_json(row))
    }

    async fn candidates_view(&mut self) -> StorageResult<Value> {
        let rows: Vec<(
            Uuid,
            Option<Uuid>,
            Uuid,
            String,
            String,
            i32,
            Option<i32>,
            String,
            i64,
            Option<i32>,
        )> = sqlx::query_as(
            "SELECT c.id, c.asset_id, c.source_event_id, c.fact_key, c.content, c.revision, \
                    c.expected_version, c.state, c.created_at, a.current_version \
             FROM oc_candidates c \
             JOIN oc_assets a ON a.id = c.asset_id AND a.tenant_id = c.tenant_id AND a.workspace_id = c.workspace_id \
             JOIN oc_events e ON e.id = c.source_event_id AND e.tenant_id = c.tenant_id AND e.workspace_id = c.workspace_id \
             WHERE c.state = 'candidate' AND c.tenant_id = ? AND c.workspace_id = ? AND NOT a.deleted AND e.state = 'active' \
             ORDER BY c.created_at, c.id LIMIT 100",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let items: Vec<Value> = rows.into_iter().map(candidate_json).collect();
        Ok(json!({ "items": items, "limit": 100 }))
    }

    async fn asset_view(&mut self, id: Uuid, version: Option<i32>) -> StorageResult<Value> {
        let row: Option<(Uuid, String, String, i32, String, Uuid, Option<i32>, String)> = sqlx::query_as(
            "SELECT a.id, a.kind, v.title, v.version, v.content, v.source_event_id, v.restored_from, v.content_hash \
             FROM oc_assets a \
             JOIN oc_versions v ON v.asset_id = a.id AND v.tenant_id = a.tenant_id AND v.workspace_id = a.workspace_id AND v.version = COALESCE(?, a.current_version) \
             JOIN oc_events e ON e.id = v.source_event_id AND e.tenant_id = v.tenant_id AND e.workspace_id = v.workspace_id \
             WHERE a.id = ? AND a.tenant_id = ? AND a.workspace_id = ? AND NOT a.deleted AND e.state = 'active'",
        )
        .bind(version)
        .bind(id)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((asset_id, kind, title, v, content, source_event_id, restored_from, content_hash)) =
            row
        else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "asset_id": asset_id,
            "kind": kind,
            "title": title,
            "version": v,
            "content": content,
            "source_event_id": source_event_id,
            "restored_from": restored_from,
            "content_hash": content_hash,
        }))
    }

    async fn job_view(&mut self, id: Uuid) -> StorageResult<Value> {
        let row: Option<(Uuid, String, String, i64, Option<String>, Option<String>, Option<String>, bool, i64)> = sqlx::query_as(
            "SELECT id, operation, state, generation, outcome, result, error_code, cancel_requested, updated_at \
             FROM oc_jobs WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((
            job_id,
            operation,
            state,
            generation,
            outcome,
            result,
            error_code,
            cancel_requested,
            updated_at,
        )) = row
        else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "id": job_id,
            "operation": operation,
            "state": state,
            "generation": generation,
            "outcome": outcome,
            "result": result.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
            "error_code": error_code,
            "cancel_requested": cancel_requested,
            "updated_at": updated_at,
        }))
    }

    async fn event_content(
        &mut self,
        source: Uuid,
        require_active: bool,
    ) -> StorageResult<Option<String>> {
        let sql = if require_active {
            "SELECT content FROM oc_events WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND state = 'active'"
        } else {
            "SELECT content FROM oc_events WHERE tenant_id = ? AND workspace_id = ? AND id = ?"
        };
        sqlx::query_scalar(sql)
            .bind(self.scope.tenant_id)
            .bind(self.scope.workspace_id)
            .bind(source)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(sqlite_err)
    }

    async fn event_file(&mut self, source: Uuid) -> StorageResult<Value> {
        let row: Option<(String, Option<Uuid>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT e.content, e.file_id, f.hash, f.media_type \
             FROM oc_events e \
             LEFT JOIN oc_files f ON f.id = e.file_id AND f.tenant_id = e.tenant_id AND f.workspace_id = e.workspace_id AND NOT f.deleted \
             WHERE e.id = ? AND e.tenant_id = ? AND e.workspace_id = ?",
        )
        .bind(source)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let Some((content, file_id, hash, media_type)) = row else {
            return Err(StorageError::NotFound);
        };
        Ok(json!({
            "content": content,
            "file_id": file_id,
            "hash": hash,
            "media_type": media_type,
        }))
    }

    async fn chunks_for_version(
        &mut self,
        asset: Uuid,
        version: i32,
    ) -> StorageResult<Vec<(String, Value)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT content, locator FROM oc_chunks \
             WHERE tenant_id = ? AND workspace_id = ? AND asset_id = ? AND version = ? ORDER BY ordinal",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(asset)
        .bind(version)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(rows
            .into_iter()
            .map(|(content, locator)| {
                (
                    content,
                    serde_json::from_str(&locator).unwrap_or(Value::Null),
                )
            })
            .collect())
    }

    async fn asset_current_version(&mut self, asset: Uuid) -> StorageResult<Option<i32>> {
        sqlx::query_scalar(
            "SELECT current_version FROM oc_assets WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(asset)
        .fetch_optional(&mut *self.tx)
        .await
        .map_err(sqlite_err)
    }

    async fn insert_version(
        &mut self,
        asset: Uuid,
        version: i32,
        content: &str,
        content_hash: &str,
        source: Uuid,
        restored_from: Option<i32>,
        title: Option<&str>,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_versions(tenant_id, workspace_id, asset_id, version, content, content_hash, source_event_id, restored_from, review_id, title, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, COALESCE(?, (SELECT title FROM oc_assets WHERE tenant_id = ? AND workspace_id = ? AND id = ?)), ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(asset)
        .bind(version)
        .bind(content)
        .bind(content_hash)
        .bind(source)
        .bind(restored_from)
        .bind(title)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(asset)
        .bind(self.principal_id)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn insert_chunk(
        &mut self,
        chunk: Uuid,
        asset: Uuid,
        version: i32,
        ordinal: i32,
        content: &str,
        locator: &Value,
        search_terms: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_chunks(tenant_id, workspace_id, id, asset_id, version, ordinal, content, locator, search_terms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(chunk)
        .bind(asset)
        .bind(version)
        .bind(ordinal)
        .bind(content)
        .bind(serde_json::to_string(locator).unwrap_or_default())
        .bind(search_terms)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn insert_summary(
        &mut self,
        summary: Uuid,
        chunk: Uuid,
        text: &str,
        model_revision: &str,
        search_terms: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_summaries(tenant_id, workspace_id, id, chunk_id, text, model_revision, search_terms) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(summary)
        .bind(chunk)
        .bind(text)
        .bind(model_revision)
        .bind(search_terms)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn update_asset_version(
        &mut self,
        asset: Uuid,
        version: i32,
        title: Option<&str>,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_assets SET current_version = ?, title = COALESCE(?, title) WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(version)
        .bind(title)
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(asset)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn active_job(
        &mut self,
        job: Uuid,
        generation: i64,
        run_token: i64,
    ) -> StorageResult<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM oc_jobs WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND generation = ? AND run_token = ? AND state = 'processing' AND NOT cancel_requested)",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(job)
        .bind(generation)
        .bind(run_token)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(sqlite_err)
    }

    async fn cleanup_derived_chunks(&mut self) -> StorageResult<u64> {
        sqlx::query(
            "DELETE FROM oc_chunks WHERE tenant_id = ? AND workspace_id = ? \
               AND (EXISTS(SELECT 1 FROM oc_assets a WHERE a.id = oc_chunks.asset_id AND a.tenant_id = oc_chunks.tenant_id AND a.workspace_id = oc_chunks.workspace_id AND a.deleted) \
                 OR EXISTS(SELECT 1 FROM oc_versions v JOIN oc_events e ON e.id = v.source_event_id AND e.tenant_id = v.tenant_id AND e.workspace_id = v.workspace_id WHERE v.asset_id = oc_chunks.asset_id AND v.version = oc_chunks.version AND v.tenant_id = oc_chunks.tenant_id AND v.workspace_id = oc_chunks.workspace_id AND e.state = 'retracted'))",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)
        .map(|r| r.rows_affected())
    }

    async fn register_owner(
        &mut self,
        source: SourceVersion,
        artifact_type: &str,
        artifact_id: Uuid,
        chunk_id: Option<Uuid>,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_artifact_owners(tenant_id, workspace_id, source_id, version, \
             artifact_type, artifact_id, chunk_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(source.source_id)
        .bind(source.version)
        .bind(artifact_type)
        .bind(artifact_id)
        .bind(chunk_id)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn detach_owners(&mut self, source: SourceVersion) -> StorageResult<u64> {
        let res = sqlx::query(
            "DELETE FROM oc_artifact_owners \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(source.source_id)
        .bind(source.version)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(res.rows_affected())
    }

    async fn owned_artifacts(&mut self) -> StorageResult<Value> {
        // Return the current owner set grouped by artifact. M4 diffs this
        // against its Kuzu rows to find orphans (graph-core G4); M3 itself does
        // not store entity/relation bodies.
        let owned: Vec<(String, Uuid)> = sqlx::query_as(
            "SELECT artifact_type, artifact_id FROM oc_artifact_owners \
             WHERE tenant_id = ? AND workspace_id = ? \
               AND artifact_type IN ('entity','relation')",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let mut relations = Vec::new();
        let mut entities = Vec::new();
        for (kind, id) in owned {
            match kind.as_str() {
                "relation" => relations.push(id),
                "entity" => entities.push(id),
                _ => {}
            }
        }
        Ok(json!({ "relations": relations, "entities": entities }))
    }

    async fn register_pending(&mut self, entry: LedgerEntry) -> StorageResult<()> {
        if entry.key.scope != self.scope {
            return Err(StorageError::Forbidden);
        }
        sqlx::query(
            "INSERT INTO oc_artifact_ledger(tenant_id, workspace_id, source_id, version, \
             artifact_type, artifact_id, surface, generation, idempotency_key, state, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(entry.key.scope.tenant_id)
        .bind(entry.key.scope.workspace_id)
        .bind(entry.key.source.source_id)
        .bind(entry.key.source.version)
        .bind(entry.artifact_type)
        .bind(entry.key.artifact_id)
        .bind(entry.key.surface.as_str())
        .bind(entry.key.generation)
        .bind(entry.idempotency_key)
        .bind(now_ms())
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn confirm_committed(&mut self, key: LedgerKey) -> StorageResult<()> {
        if key.scope != self.scope {
            return Err(StorageError::Forbidden);
        }
        // Re-check source validity and generation in the same statement (A2.6
        // step 3): only an active source's matching generation commits.
        sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'committed', updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ? \
               AND state IN ('pending','retry_wait') \
               AND EXISTS (SELECT 1 FROM oc_events e \
                           WHERE e.tenant_id = oc_artifact_ledger.tenant_id \
                             AND e.workspace_id = oc_artifact_ledger.workspace_id \
                             AND e.id = oc_artifact_ledger.source_id \
                             AND e.state = 'active')",
        )
        .bind(now_ms())
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn fail_retryable(&mut self, key: LedgerKey, error: &str) -> StorageResult<()> {
        if key.scope != self.scope {
            return Err(StorageError::Forbidden);
        }
        let now = now_ms();
        let attempt: i32 = sqlx::query_scalar(
            "SELECT attempt FROM oc_artifact_ledger \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ?",
        )
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let next = attempt + 1;
        if next >= MAX_LEDGER_ATTEMPTS {
            sqlx::query(
                "UPDATE oc_artifact_ledger SET state = 'orphan', attempt = ?, last_error = ?, \
                 next_retry_at = NULL, updated_at = ? \
                 WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
                   AND artifact_id = ? AND surface = ? AND generation = ? \
                   AND state IN ('pending','retry_wait')",
            )
            .bind(next)
            .bind(error)
            .bind(now)
            .bind(key.scope.tenant_id)
            .bind(key.scope.workspace_id)
            .bind(key.source.source_id)
            .bind(key.source.version)
            .bind(key.artifact_id)
            .bind(key.surface.as_str())
            .bind(key.generation)
            .execute(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
        } else {
            sqlx::query(
                "UPDATE oc_artifact_ledger SET state = 'retry_wait', attempt = ?, last_error = ?, \
                 next_retry_at = ?, updated_at = ? \
                 WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
                   AND artifact_id = ? AND surface = ? AND generation = ? \
                   AND state IN ('pending','retry_wait')",
            )
            .bind(next)
            .bind(error)
            .bind(now + retry_delay(next))
            .bind(now)
            .bind(key.scope.tenant_id)
            .bind(key.scope.workspace_id)
            .bind(key.source.source_id)
            .bind(key.source.version)
            .bind(key.artifact_id)
            .bind(key.surface.as_str())
            .bind(key.generation)
            .execute(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
        }
        Ok(())
    }

    async fn fail_permanent(&mut self, key: LedgerKey, error: &str) -> StorageResult<()> {
        if key.scope != self.scope {
            return Err(StorageError::Forbidden);
        }
        sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'orphan', last_error = ?, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ? \
               AND state IN ('pending','retry_wait')",
        )
        .bind(error)
        .bind(now_ms())
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn settle_completed(
        &mut self,
        job: Uuid,
        outcome: &str,
        result: &Value,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_jobs SET state = 'completed', outcome = ?, result = ?, error_code = NULL, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ?",
        )
        .bind(outcome)
        .bind(serde_json::to_string(result).unwrap_or_default())
        .bind(now_ms())
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(job)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn settle_failed(
        &mut self,
        job: Uuid,
        generation: i64,
        run_token: i64,
        state: &str,
        error_code: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_jobs SET state = ?, error_code = ?, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND generation = ? AND run_token = ? AND state = 'processing'",
        )
        .bind(state)
        .bind(error_code)
        .bind(now_ms())
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(job)
        .bind(generation)
        .bind(run_token)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn commit(self) -> StorageResult<()> {
        if !self.required.is_empty() {
            let role = self.live_role().await?;
            for permission in &self.required {
                if !permission.granted_by(&role) {
                    return Err(StorageError::Forbidden);
                }
            }
        }
        self.tx.commit().await.map_err(sqlite_err)
    }

    async fn rollback(self) -> StorageResult<()> {
        self.tx.rollback().await.map_err(sqlite_err)
    }
}

impl RelationalStore for SqliteStore {
    type Tx = SqliteTx;

    async fn begin(&self, authorized: AuthorizedScope) -> StorageResult<Self::Tx> {
        let mut tx = self.pool.begin().await.map_err(sqlite_err)?;
        // Reject a revoked or unknown principal before handing out a
        // transaction; the commit re-check re-verifies against a fresh read.
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM oc_api_keys WHERE id = ? AND NOT revoked")
                .bind(authorized.principal_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlite_err)?;
        role.ok_or(StorageError::Forbidden)?;
        Ok(SqliteTx {
            tx,
            pool: self.pool.clone(),
            scope: authorized.scope,
            principal_id: authorized.principal_id,
            required: Vec::new(),
            command: None,
        })
    }

    async fn authenticate(&self, token: &str) -> StorageResult<AuthorizedScope> {
        if token.len() < 32 || token.len() > 256 {
            return Err(StorageError::Forbidden);
        }
        let row: Option<(Uuid, Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT tenant_id, workspace_id, id, role FROM oc_api_keys WHERE token_hash = ? AND NOT revoked",
        )
        .bind(hash(token))
        .fetch_optional(&self.pool)
        .await
        .map_err(sqlite_err)?;
        let (tenant_id, workspace_id, principal_id, role) = row.ok_or(StorageError::Forbidden)?;
        Ok(AuthorizedScope {
            scope: Scope {
                tenant_id,
                workspace_id,
            },
            principal_id,
            role,
        })
    }

    async fn create_workspace(
        &self,
        tenant_id: Uuid,
        workspace_id: Uuid,
        name: &str,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_workspaces(tenant_id, id, name, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(name)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn issue_key(&self, scope: Scope, role: &str) -> StorageResult<IssuedKey> {
        if !ROLES.contains(&role) {
            return Err(StorageError::Conflict(format!("invalid role {role}")));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM oc_workspaces WHERE tenant_id = ? AND id = ?)",
        )
        .bind(scope.tenant_id)
        .bind(scope.workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlite_err)?;
        if !exists {
            return Err(StorageError::NotFound);
        }
        let key_id = Uuid::new_v4();
        let token = format!("oc_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO oc_api_keys(id, tenant_id, workspace_id, token_hash, role, revoked, created_at) \
             VALUES (?, ?, ?, ?, ?, 0, ?)",
        )
        .bind(key_id)
        .bind(scope.tenant_id)
        .bind(scope.workspace_id)
        .bind(hash(&token))
        .bind(role)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?;
        Ok(IssuedKey { key_id, token })
    }

    async fn revoke_key(&self, key_id: Uuid) -> StorageResult<()> {
        sqlx::query("UPDATE oc_api_keys SET revoked = 1 WHERE id = ?")
            .bind(key_id)
            .execute(&self.pool)
            .await
            .map_err(sqlite_err)?;
        Ok(())
    }

    async fn reconcile_ledger(&self) -> StorageResult<Value> {
        let now = now_ms();
        let orphaned = sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'orphan', updated_at = ? \
             WHERE state = 'pending' AND updated_at < ?",
        )
        .bind(now)
        .bind(now - LEDGER_PENDING_TIMEOUT_MS)
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?
        .rows_affected();
        let requeued = sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'pending', updated_at = ? \
             WHERE state = 'retry_wait' AND next_retry_at IS NOT NULL AND next_retry_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?
        .rows_affected();
        Ok(json!({ "requeued": requeued, "orphaned": orphaned }))
    }
}

impl JobQueue for SqliteStore {
    async fn claim_next(&self) -> StorageResult<Option<ClaimedJob>> {
        let mut tx = self.pool.begin().await.map_err(sqlite_err)?;
        let now = now_ms();
        // Requeue processing jobs abandoned by a crashed worker (A2.5).
        sqlx::query(
            "UPDATE oc_jobs SET state = 'pending', updated_at = ? \
             WHERE state = 'processing' AND NOT cancel_requested AND updated_at < ?",
        )
        .bind(now)
        .bind(now - PROCESSING_TIMEOUT_MS)
        .execute(&mut *tx)
        .await
        .map_err(sqlite_err)?;
        let row: Option<(
            Uuid,
            Uuid,
            Uuid,
            String,
            String,
            i64,
            i64,
            Uuid,
            Option<Uuid>,
            Option<Uuid>,
        )> = sqlx::query_as(
            "SELECT id, tenant_id, workspace_id, operation, payload, generation, run_token, \
                    created_by, asset_id, source_event_id \
             FROM oc_jobs \
             WHERE state IN ('pending', 'retry_wait') AND NOT cancel_requested \
               AND (next_retry_at IS NULL OR next_retry_at <= ?) \
             ORDER BY created_at LIMIT 1",
        )
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlite_err)?;
        let Some((
            job_id,
            tenant_id,
            workspace_id,
            kind,
            payload,
            generation,
            run_token,
            created_by,
            asset,
            source,
        )) = row
        else {
            tx.commit().await.map_err(sqlite_err)?;
            return Ok(None);
        };
        // The state guard makes the claim atomic even if two workers race.
        let claimed = sqlx::query(
            "UPDATE oc_jobs SET state = 'processing', run_token = run_token + 1, \
             attempt = attempt + 1, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND state IN ('pending', 'retry_wait')",
        )
        .bind(now)
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(job_id)
        .execute(&mut *tx)
        .await
        .map_err(sqlite_err)?;
        tx.commit().await.map_err(sqlite_err)?;
        if claimed.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(ClaimedJob {
            scope: Scope {
                tenant_id,
                workspace_id,
            },
            job_id,
            kind,
            payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            generation,
            run_token: run_token + 1,
            created_by,
            asset,
            source,
        }))
    }

    async fn settle(&self, claim: ClaimedJob, finish: JobFinish) -> StorageResult<()> {
        let now = now_ms();
        let (state, outcome, attempt, next_retry_at) = match finish {
            JobFinish::Completed => ("completed", Some("ok"), None, None),
            JobFinish::Failed => ("failed", Some("failed"), None, None),
            JobFinish::Cancelled => ("cancelled", Some("cancelled"), None, None),
            JobFinish::Retryable { attempt } => (
                "retry_wait",
                None,
                Some(attempt),
                Some(now + retry_delay(attempt)),
            ),
        };
        let result = sqlx::query(
            "UPDATE oc_jobs SET state = ?, outcome = ?, attempt = COALESCE(?, attempt), \
             next_retry_at = ?, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND id = ? AND generation = ? AND run_token = ?",
        )
        .bind(state)
        .bind(outcome)
        .bind(attempt)
        .bind(next_retry_at)
        .bind(now)
        .bind(claim.scope.tenant_id)
        .bind(claim.scope.workspace_id)
        .bind(claim.job_id)
        .bind(claim.generation)
        .bind(claim.run_token)
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?;
        if result.rows_affected() == 0 {
            return Err(StorageError::Conflict("stale job claim".to_string()));
        }
        Ok(())
    }
}
