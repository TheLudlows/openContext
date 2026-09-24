//! Domain storage interfaces.
//!
//! Nothing here mentions a pool, SQL or a vendor type; the domain talks to
//! these traits only (A2.2). The call sequence for a single write (A2.2) is:
//!
//! ```text
//! let mut tx = relational.begin(authorized_scope).await?;   // carries scope
//! tx.check_permission(Permission::Write).await?;            // required now
//! if let Some(cached) = tx.begin_command("memory.create", idem_key, req_hash).await? {
//!     return cached;                                        // idempotent replay
//! }
//! tx.record_source(source_version).await?;
//! tx.enqueue(work_item).await?;                             // same transaction
//! tx.audit("memory.create", target, detail).await?;
//! // ... any model / file IO happens here, OUTSIDE the transaction ...
//! tx.finish_command(response).await?;
//! tx.commit().await?;                                       // re-checks permission
//! ```
//!
//! Any failure before `commit` rolls the whole transaction back; external IO
//! must not run while the transaction is open, and `commit` re-verifies
//! permission against the live role.

// Native async-fn-in-trait (AFIT, edition 2024) is intentional here: these
// traits are used behind generics (not `dyn`), so `Send`-ness of the returned
// futures is decided by each concrete adapter, not the trait.
#![allow(async_fn_in_trait, clippy::too_many_arguments)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::storage::ledger::{LedgerEntry, LedgerKey};
use crate::storage::{
    capabilities::{BlobKey, Capabilities, VectorEntry, VectorHit, VectorQuery},
    error::StorageResult,
    scope::{AuthorizedScope, Permission, Scope, SourceVersion},
};
use crate::types::{Entity, Relation};

/// A unit of work registered on a transaction for later claim by a worker.
/// The queue consumer claims/acks/retries; enqueue itself never opens a second
/// connection (A2.2, A2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub job_id: Uuid,
    pub kind: String,
    pub payload: Value,
    pub asset: Option<Uuid>,
    pub source: Option<Uuid>,
}

/// The plaintext API key material returned once at issue time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedKey {
    pub key_id: Uuid,
    pub token: String,
}

/// A scoped domain transaction. It carries its [`Scope`] and exposes business
/// operations only — every object read or written below binds the transaction's
/// tenant/workspace, and no SQL, pool or vendor type leaks through (A2.2). The
/// concrete operation set mirrors the A5 objects: events, memory/knowledge
/// assets, candidates, reviews, versions, chunks, summaries, files and jobs.
///
/// `commit`/`rollback` consume the transaction so it cannot be used after
/// settlement.
pub trait DomainTx {
    /// The scope this transaction is bound to.
    fn scope(&self) -> Scope;

    /// Check that the current principal still holds `permission` now, and
    /// record it so `commit` re-verifies it against the live role (A2.2:
    /// permission is re-checked at commit).
    async fn check_permission(&mut self, permission: Permission) -> StorageResult<()>;

    /// Open an idempotent command. A repeated `idempotency_key` with the same
    /// `request_hash` returns the previously recorded response without
    /// re-applying side effects; the same key with a different hash is a
    /// conflict. `None` means this is the first execution (A2.6).
    async fn begin_command(
        &mut self,
        operation: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> StorageResult<Option<Value>>;

    /// Register a source version as the origin of this command's writes.
    async fn record_source(&mut self, source: SourceVersion) -> StorageResult<()>;

    /// Register work on this same transaction. Business write and enqueue
    /// commit or roll back together; a consumer (not this method) claims it.
    async fn enqueue(&mut self, work: WorkItem) -> StorageResult<()>;

    /// Record an audit event targeting `target` on this transaction.
    async fn audit(&mut self, action: &str, target: Uuid, detail: Value) -> StorageResult<()>;

    /// Record the command's response and close it.
    async fn finish_command(&mut self, response: Value) -> StorageResult<()>;

    // -- events & assets ------------------------------------------------------

    /// Insert an event, returning its id. `file` links a prior upload.
    async fn create_event(
        &mut self,
        kind: &str,
        content: &str,
        file: Option<Uuid>,
    ) -> StorageResult<Uuid>;

    /// Get-or-create the memory asset bound to `fact_key` (the fact slot),
    /// returning its id and current version (`None` on first creation).
    async fn slot(&mut self, fact_key: &str) -> StorageResult<(Uuid, Option<i32>)>;

    /// Insert a candidate for `asset` originating from `source`, returning its
    /// id. `approved` selects the initial state (`approved` vs `candidate`).
    async fn insert_candidate(
        &mut self,
        asset: Uuid,
        fact_key: &str,
        content: &str,
        source: Uuid,
        approved: bool,
        expected_version: Option<i32>,
    ) -> StorageResult<Uuid>;

    /// Insert a new `knowledge` asset with `title`, returning its id.
    async fn insert_knowledge_asset(&mut self, title: &str) -> StorageResult<Uuid>;

    // -- files ----------------------------------------------------------------

    /// Record a file's metadata, returning its id.
    async fn insert_file(
        &mut self,
        name: &str,
        media_type: &str,
        hash: &str,
        size: i64,
    ) -> StorageResult<Uuid>;

    /// Whether a non-deleted file with `id` exists.
    async fn file_exists(&mut self, id: Uuid) -> StorageResult<bool>;

    /// The stored hash of a non-deleted file, if any.
    async fn file_hash(&mut self, id: Uuid) -> StorageResult<Option<String>>;

    /// Whether a non-deleted file with `id` is still visible.
    async fn file_visible(&mut self, id: Uuid) -> StorageResult<bool>;

    // -- review ---------------------------------------------------------------

    /// A candidate joined with its asset/source state, or [`StorageError::NotFound`].
    async fn candidate_for_review(&mut self, id: Uuid) -> StorageResult<Value>;

    /// Record a review and move the candidate to `approved`/`rejected`.
    async fn apply_review(
        &mut self,
        review: Uuid,
        candidate: Uuid,
        revision: i32,
        decision: &str,
        expected_version: Option<i32>,
        reason: &str,
    ) -> StorageResult<()>;

    // -- restore --------------------------------------------------------------

    /// The source event, title and current version of `asset` at `version`,
    /// restricted to a live source, or [`StorageError::NotFound`].
    async fn restore_source(&mut self, asset: Uuid, version: i32) -> StorageResult<Value>;

    // -- deletes --------------------------------------------------------------

    /// Soft-delete an asset; returns the number of rows affected.
    async fn delete_asset(&mut self, id: Uuid) -> StorageResult<u64>;

    /// Retract an event; returns the number of rows affected.
    async fn retract_event(&mut self, id: Uuid) -> StorageResult<u64>;

    /// Soft-delete a file and retract its events; returns rows affected.
    async fn delete_file(&mut self, id: Uuid) -> StorageResult<u64>;

    /// Withdraw candidates whose asset/source is now deleted or retracted.
    async fn withdraw_affected_candidates(&mut self) -> StorageResult<()>;

    /// Cancel jobs whose asset/source is now deleted or retracted.
    async fn cancel_affected_jobs(&mut self) -> StorageResult<()>;

    // -- jobs -----------------------------------------------------------------

    /// A job row (creator, operation, state, targets) or [`StorageError::NotFound`].
    async fn job_row(&mut self, id: Uuid) -> StorageResult<Value>;

    /// Apply a `cancel`/`retry` action to a job, returning its new generation.
    async fn set_job_action(&mut self, id: Uuid, action: &str) -> StorageResult<i64>;

    // -- target validity ------------------------------------------------------

    /// Fail with [`StorageError::Conflict`] if `asset`/`source` is missing,
    /// deleted or retracted (A2.4 source-validity re-check).
    async fn valid_targets(
        &mut self,
        asset: Option<Uuid>,
        source: Option<Uuid>,
    ) -> StorageResult<()>;

    // -- API read views -------------------------------------------------------

    /// A candidate plus its asset `current_version`, or [`StorageError::NotFound`].
    async fn candidate_view(&mut self, id: Uuid) -> StorageResult<Value>;

    /// Up to 100 open candidates as an `{"items": [...], "limit": 100}` value.
    async fn candidates_view(&mut self) -> StorageResult<Value>;

    /// A published asset version, or [`StorageError::NotFound`].
    async fn asset_view(&mut self, id: Uuid, version: Option<i32>) -> StorageResult<Value>;

    /// A job's public state, or [`StorageError::NotFound`].
    async fn job_view(&mut self, id: Uuid) -> StorageResult<Value>;

    // -- worker reads & writes ------------------------------------------------

    /// A source event's content, optionally restricted to `active` state.
    async fn event_content(
        &mut self,
        source: Uuid,
        require_active: bool,
    ) -> StorageResult<Option<String>>;

    /// A source event joined with its file for ingest, or [`StorageError::NotFound`].
    async fn event_file(&mut self, source: Uuid) -> StorageResult<Value>;

    /// The ordered `(content, locator)` chunks of an asset version.
    async fn chunks_for_version(
        &mut self,
        asset: Uuid,
        version: i32,
    ) -> StorageResult<Vec<(String, Value)>>;

    /// The current published version of an asset, if any.
    async fn asset_current_version(&mut self, asset: Uuid) -> StorageResult<Option<i32>>;

    /// Insert a new version of `asset`.
    async fn insert_version(
        &mut self,
        asset: Uuid,
        version: i32,
        content: &str,
        content_hash: &str,
        source: Uuid,
        restored_from: Option<i32>,
        title: Option<&str>,
    ) -> StorageResult<()>;

    /// Insert one chunk of a version.
    async fn insert_chunk(
        &mut self,
        chunk: Uuid,
        asset: Uuid,
        version: i32,
        ordinal: i32,
        content: &str,
        locator: &Value,
        search_terms: &str,
    ) -> StorageResult<()>;

    /// Insert one chunk summary.
    async fn insert_summary(
        &mut self,
        summary: Uuid,
        chunk: Uuid,
        text: &str,
        model_revision: &str,
        search_terms: &str,
    ) -> StorageResult<()>;

    /// Advance an asset's current version (and optionally its title).
    async fn update_asset_version(
        &mut self,
        asset: Uuid,
        version: i32,
        title: Option<&str>,
    ) -> StorageResult<()>;

    /// Whether a job is still `processing` under the given generation/run_token.
    async fn active_job(
        &mut self,
        job: Uuid,
        generation: i64,
        run_token: i64,
    ) -> StorageResult<bool>;

    /// Delete derived chunks of deleted/retracted sources; returns rows removed.
    async fn cleanup_derived_chunks(&mut self) -> StorageResult<u64>;

    /// Mark a job `completed` with its outcome and result.
    async fn settle_completed(
        &mut self,
        job: Uuid,
        outcome: &str,
        result: &Value,
    ) -> StorageResult<()>;

    /// Mark a processing job `superseded`/`failed` with an error code.
    async fn settle_failed(
        &mut self,
        job: Uuid,
        generation: i64,
        run_token: i64,
        state: &str,
        error_code: &str,
    ) -> StorageResult<()>;

    // -- provenance (M3) -----------------------------------------------------

    /// Record that `artifact_id` (a chunk/summary/entity/relation) originates
    /// from `source`. `chunk_id` is set only for chunk/summary artifacts.
    async fn register_owner(
        &mut self,
        source: SourceVersion,
        artifact_type: &str,
        artifact_id: Uuid,
        chunk_id: Option<Uuid>,
    ) -> StorageResult<()>;

    /// Remove every owner row for `source`, returning rows removed. Called
    /// when a source is retracted so derived artifacts can be reaped.
    async fn detach_owners(&mut self, source: SourceVersion) -> StorageResult<u64>;

    /// The currently-owned entity/relation ids, as
    /// `{"relations": [..], "entities": [..]}`. The graph adapter (M4) diffs
    /// this against its own rows to find orphans (graph-core G4); artifacts
    /// shared with another source stay in this set.
    async fn owned_artifacts(&mut self) -> StorageResult<Value>;

    // -- cross-store ledger (M3) ---------------------------------------------

    /// Register a pending cross-store write on this transaction (A2.6 step 1).
    /// Idempotent: a repeated key leaves the existing row untouched.
    async fn register_pending(&mut self, entry: LedgerEntry) -> StorageResult<()>;

    /// Confirm a pending write committed, re-checking source validity and
    /// generation in the same statement (A2.6 step 3). No-op (idempotent) if
    /// the entry is already committed, the source is retracted, or its
    /// generation changed.
    async fn confirm_committed(&mut self, key: LedgerKey) -> StorageResult<()>;

    /// Record a retryable external-write failure; becomes `orphan` after the
    /// retry budget is exhausted (A2.6).
    async fn fail_retryable(&mut self, key: LedgerKey, error: &str) -> StorageResult<()>;

    /// Record a permanent external-write failure as `orphan` (A2.6).
    async fn fail_permanent(&mut self, key: LedgerKey, error: &str) -> StorageResult<()>;

    /// Commit all registered writes atomically, re-verifying recorded
    /// permissions against the live role first.
    async fn commit(self) -> StorageResult<()>;

    /// Abort; nothing registered is written.
    async fn rollback(self) -> StorageResult<()>;
}

/// The scoped business-transaction store: sources, assets, versions, identity
/// re-verification, idempotency, audit, job registration and ledger (A2.2).
pub trait RelationalStore {
    /// The concrete transaction type.
    type Tx: DomainTx;

    /// Open a scoped transaction for an authenticated principal.
    async fn begin(&self, authorized: AuthorizedScope) -> StorageResult<Self::Tx>;

    /// Privileged: resolve a bearer token to a principal+scope.
    async fn authenticate(&self, token: &str) -> StorageResult<AuthorizedScope>;

    /// Privileged: create a named workspace (platform admin only).
    async fn create_workspace(
        &self,
        tenant_id: Uuid,
        workspace_id: Uuid,
        name: &str,
    ) -> StorageResult<()>;

    /// Privileged: issue an API key for a workspace, returning it once.
    async fn issue_key(&self, scope: Scope, role: &str) -> StorageResult<IssuedKey>;

    /// Privileged: revoke a key by id.
    async fn revoke_key(&self, key_id: Uuid) -> StorageResult<()>;

    /// Reconcile the cross-store ledger on startup/recovery: abandon stuck
    /// pending writes as orphan and requeue due retry_wait writes (A2.6).
    /// Returns counts as `{"requeued": n, "orphaned": n}`.
    async fn reconcile_ledger(&self) -> StorageResult<Value>;
}

/// A job claimed by a worker, carrying the fields needed to re-verify the
/// claim at settle time (scope, generation, run_token) and to reconstruct the
/// authorizing principal and targets (creator, asset, source) (A2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedJob {
    pub scope: Scope,
    pub job_id: Uuid,
    pub kind: String,
    pub payload: Value,
    pub generation: i64,
    pub run_token: i64,
    pub created_by: Uuid,
    pub asset: Option<Uuid>,
    pub source: Option<Uuid>,
}

/// How a worker settles a claimed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFinish {
    Completed,
    Retryable { attempt: i32 },
    Failed,
    Cancelled,
}

/// The consumer side of the job queue. `claim_next` is privileged/global;
/// business writes reach the queue only through [`DomainTx::enqueue`] on the
/// same transaction. The consumer claims, acknowledges and retries — it never
/// enqueues on a second connection (A2.2, A2.5).
pub trait JobQueue {
    /// Atomically claim the next runnable job across all scopes.
    async fn claim_next(&self) -> StorageResult<Option<ClaimedJob>>;

    /// Settle a previously claimed job (complete, retry, fail or cancel),
    /// re-verifying scope/generation/run_token before applying.
    async fn settle(&self, claim: ClaimedJob, finish: JobFinish) -> StorageResult<()>;
}

/// Vector storage. Every query carries scope + profile + dimension +
/// generation; writes carry scope + source version (A2.2).
pub trait VectorStore {
    /// Declared capabilities (A2.7).
    fn capabilities(&self) -> Capabilities;

    /// Idempotently write entries for a scope/profile/generation.
    async fn upsert(&self, entries: Vec<VectorEntry>) -> StorageResult<()>;

    /// Scoped nearest-neighbour search.
    async fn search(&self, query: VectorQuery) -> StorageResult<Vec<VectorHit>>;

    /// Remove every vector originating from a source version.
    async fn delete_source(&self, scope: Scope, source: SourceVersion) -> StorageResult<()>;
}

/// Graph storage. Entity/relation writes carry scope + source version; queries
/// declare the max traversal they need (A2.2, A2.7).
pub trait GraphStore {
    /// Declared capabilities (A2.7).
    fn capabilities(&self) -> Capabilities;

    /// Write entities originating from a source version.
    async fn upsert_entities(
        &self,
        scope: Scope,
        source: SourceVersion,
        entities: Vec<Entity>,
    ) -> StorageResult<()>;

    /// Write relations originating from a source version.
    async fn upsert_relations(
        &self,
        scope: Scope,
        source: SourceVersion,
        relations: Vec<Relation>,
    ) -> StorageResult<()>;

    /// Return the ids reachable from `from` within `max_hops` hops.
    async fn traverse(&self, scope: Scope, from: Uuid, max_hops: usize)
    -> StorageResult<Vec<Uuid>>;

    /// Remove graph objects that originate solely from a source version.
    async fn delete_source(&self, scope: Scope, source: SourceVersion) -> StorageResult<()>;
}

/// Raw file storage. Keys are [`BlobKey`]s constrained by scope + root, never
/// free-form paths (A2.5).
pub trait BlobStore {
    async fn put(&self, key: &BlobKey, data: Vec<u8>) -> StorageResult<()>;
    async fn get(&self, key: &BlobKey) -> StorageResult<Vec<u8>>;
    async fn delete(&self, key: &BlobKey) -> StorageResult<()>;
}

/// Lifecycle shared by every backend (A2.3). `initialize` creates the current
/// structure only when absent and is idempotent; `check` fails on missing or
/// incompatible structure without repairing; `shutdown` releases resources.
pub trait Lifecycle {
    async fn initialize(&self) -> StorageResult<()>;
    async fn check(&self) -> StorageResult<()>;
    async fn shutdown(&self) -> StorageResult<()>;
}
