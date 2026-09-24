//! Reusable storage-contract tests (M1).
//!
//! The contract functions below are written against the traits, not a concrete
//! backend: a future SQLite/PostgreSQL adapter repeats them by constructing its
//! own `RelationalStore` + `JobQueue` and calling the same functions with the
//! same assertions. The in-memory double here only proves the contract is
//! coherent today.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use uuid::Uuid;

use opencontext::storage::ledger::{LedgerEntry, LedgerKey};
use opencontext::storage::{
    AuthorizedScope, ClaimedJob, DomainTx, IssuedKey, JobFinish, JobQueue, Permission,
    RelationalStore, Scope, SourceVersion, StorageError, StorageResult, WorkItem,
};

// ---------------------------------------------------------------------------
// In-memory double
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemDb {
    roles: HashMap<Uuid, String>,
    commands: HashMap<String, (String, Value)>,
    jobs: Vec<ClaimedJob>,
    audits: Vec<String>,
    sources: Vec<SourceVersion>,
}

struct MemRelationalStore {
    db: Arc<Mutex<MemDb>>,
}

struct MemQueue {
    db: Arc<Mutex<MemDb>>,
}

struct MemTx {
    scope: Scope,
    principal: Uuid,
    db: Arc<Mutex<MemDb>>,
    idempotency_key: Option<String>,
    request_hash: Option<String>,
    response: Option<Value>,
    work: Vec<WorkItem>,
    audits: Vec<String>,
    sources: Vec<SourceVersion>,
    required: Vec<Permission>,
}

impl MemTx {
    fn role(&self) -> StorageResult<String> {
        self.db
            .lock()
            .unwrap()
            .roles
            .get(&self.principal)
            .cloned()
            .ok_or(StorageError::Forbidden)
    }
}

impl DomainTx for MemTx {
    fn scope(&self) -> Scope {
        self.scope
    }

    async fn check_permission(&mut self, permission: Permission) -> StorageResult<()> {
        if permission.granted_by(&self.role()?) {
            self.required.push(permission);
            Ok(())
        } else {
            Err(StorageError::Forbidden)
        }
    }

    async fn begin_command(
        &mut self,
        _operation: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> StorageResult<Option<Value>> {
        let db = self.db.lock().unwrap();
        if let Some((hash, response)) = db.commands.get(idempotency_key) {
            if hash == request_hash {
                return Ok(Some(response.clone()));
            }
            return Err(StorageError::Conflict(format!(
                "idempotency key {idempotency_key} reused with different input"
            )));
        }
        drop(db);
        self.idempotency_key = Some(idempotency_key.to_string());
        self.request_hash = Some(request_hash.to_string());
        Ok(None)
    }

    async fn record_source(&mut self, source: SourceVersion) -> StorageResult<()> {
        self.sources.push(source);
        Ok(())
    }

    async fn register_owner(
        &mut self,
        _source: SourceVersion,
        _artifact_type: &str,
        _artifact_id: Uuid,
        _chunk_id: Option<Uuid>,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }

    async fn detach_owners(&mut self, _source: SourceVersion) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }

    async fn owned_artifacts(&mut self) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }

    async fn register_pending(&mut self, _entry: LedgerEntry) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn confirm_committed(&mut self, _key: LedgerKey) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn fail_retryable(&mut self, _key: LedgerKey, _error: &str) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn fail_permanent(&mut self, _key: LedgerKey, _error: &str) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn enqueue(&mut self, work: WorkItem) -> StorageResult<()> {
        self.work.push(work);
        Ok(())
    }

    async fn audit(&mut self, action: &str, _target: Uuid, detail: Value) -> StorageResult<()> {
        self.audits.push(format!("{action}:{detail}"));
        Ok(())
    }

    async fn finish_command(&mut self, response: Value) -> StorageResult<()> {
        self.response = Some(response);
        Ok(())
    }

    // The in-memory double exercises the transaction envelope only; domain rows
    // are not modeled, so the concrete operations below are unavailable.
    async fn create_event(
        &mut self,
        _kind: &str,
        _content: &str,
        _file: Option<Uuid>,
    ) -> StorageResult<Uuid> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn slot(&mut self, _fact_key: &str) -> StorageResult<(Uuid, Option<i32>)> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_candidate(
        &mut self,
        _asset: Uuid,
        _fact_key: &str,
        _content: &str,
        _source: Uuid,
        _approved: bool,
        _expected_version: Option<i32>,
    ) -> StorageResult<Uuid> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_knowledge_asset(&mut self, _title: &str) -> StorageResult<Uuid> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_file(
        &mut self,
        _name: &str,
        _media_type: &str,
        _hash: &str,
        _size: i64,
    ) -> StorageResult<Uuid> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn file_exists(&mut self, _id: Uuid) -> StorageResult<bool> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn file_hash(&mut self, _id: Uuid) -> StorageResult<Option<String>> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn file_visible(&mut self, _id: Uuid) -> StorageResult<bool> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn candidate_for_review(&mut self, _id: Uuid) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn apply_review(
        &mut self,
        _review: Uuid,
        _candidate: Uuid,
        _revision: i32,
        _decision: &str,
        _expected_version: Option<i32>,
        _reason: &str,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn restore_source(&mut self, _asset: Uuid, _version: i32) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn delete_asset(&mut self, _id: Uuid) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn retract_event(&mut self, _id: Uuid) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn delete_file(&mut self, _id: Uuid) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn withdraw_affected_candidates(&mut self) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn cancel_affected_jobs(&mut self) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn job_row(&mut self, _id: Uuid) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn set_job_action(&mut self, _id: Uuid, _action: &str) -> StorageResult<i64> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn valid_targets(
        &mut self,
        _asset: Option<Uuid>,
        _source: Option<Uuid>,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn candidate_view(&mut self, _id: Uuid) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn candidates_view(&mut self) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn asset_view(&mut self, _id: Uuid, _version: Option<i32>) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn job_view(&mut self, _id: Uuid) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn event_content(
        &mut self,
        _source: Uuid,
        _require_active: bool,
    ) -> StorageResult<Option<String>> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn event_file(&mut self, _source: Uuid) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn chunks_for_version(
        &mut self,
        _asset: Uuid,
        _version: i32,
    ) -> StorageResult<Vec<(String, Value)>> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn asset_current_version(&mut self, _asset: Uuid) -> StorageResult<Option<i32>> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_version(
        &mut self,
        _asset: Uuid,
        _version: i32,
        _content: &str,
        _content_hash: &str,
        _source: Uuid,
        _restored_from: Option<i32>,
        _title: Option<&str>,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_chunk(
        &mut self,
        _chunk: Uuid,
        _asset: Uuid,
        _version: i32,
        _ordinal: i32,
        _content: &str,
        _locator: &Value,
        _search_terms: &str,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn insert_summary(
        &mut self,
        _summary: Uuid,
        _chunk: Uuid,
        _text: &str,
        _model_revision: &str,
        _search_terms: &str,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn update_asset_version(
        &mut self,
        _asset: Uuid,
        _version: i32,
        _title: Option<&str>,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn active_job(
        &mut self,
        _job: Uuid,
        _generation: i64,
        _run_token: i64,
    ) -> StorageResult<bool> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn cleanup_derived_chunks(&mut self) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn settle_completed(
        &mut self,
        _job: Uuid,
        _outcome: &str,
        _result: &Value,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }
    async fn settle_failed(
        &mut self,
        _job: Uuid,
        _generation: i64,
        _run_token: i64,
        _state: &str,
        _error_code: &str,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no domain rows".into(),
        ))
    }

    async fn commit(mut self) -> StorageResult<()> {
        for &permission in &self.required {
            if !permission.granted_by(&self.role()?) {
                return Err(StorageError::Forbidden);
            }
        }
        let mut db = self.db.lock().unwrap();
        if let Some(key) = self.idempotency_key.take() {
            db.commands.insert(
                key,
                (
                    self.request_hash.take().unwrap_or_default(),
                    self.response.clone().unwrap_or(Value::Null),
                ),
            );
        }
        for work in self.work {
            db.jobs.push(ClaimedJob {
                scope: self.scope,
                job_id: work.job_id,
                kind: work.kind,
                payload: work.payload,
                generation: 1,
                run_token: 1,
                created_by: self.principal,
                asset: work.asset,
                source: work.source,
            });
        }
        db.audits.extend(self.audits);
        db.sources.extend(self.sources);
        Ok(())
    }

    async fn rollback(self) -> StorageResult<()> {
        Ok(())
    }
}

impl RelationalStore for MemRelationalStore {
    type Tx = MemTx;

    async fn begin(&self, authorized: AuthorizedScope) -> StorageResult<Self::Tx> {
        if !self
            .db
            .lock()
            .unwrap()
            .roles
            .contains_key(&authorized.principal_id)
        {
            return Err(StorageError::Forbidden);
        }
        Ok(MemTx {
            scope: authorized.scope,
            principal: authorized.principal_id,
            db: self.db.clone(),
            idempotency_key: None,
            request_hash: None,
            response: None,
            work: Vec::new(),
            audits: Vec::new(),
            sources: Vec::new(),
            required: Vec::new(),
        })
    }

    async fn authenticate(&self, _token: &str) -> StorageResult<AuthorizedScope> {
        Err(StorageError::Unavailable(
            "in-memory double has no tokens".into(),
        ))
    }

    async fn create_workspace(
        &self,
        _tenant_id: Uuid,
        _workspace_id: Uuid,
        _name: &str,
    ) -> StorageResult<()> {
        Ok(())
    }

    async fn issue_key(&self, _scope: Scope, _role: &str) -> StorageResult<IssuedKey> {
        Err(StorageError::Unavailable(
            "in-memory double has no keys".into(),
        ))
    }

    async fn revoke_key(&self, _key_id: Uuid) -> StorageResult<()> {
        Ok(())
    }

    async fn reconcile_ledger(&self) -> StorageResult<Value> {
        Ok(serde_json::json!({"requeued": 0, "orphaned": 0}))
    }
}

impl JobQueue for MemQueue {
    async fn claim_next(&self) -> StorageResult<Option<ClaimedJob>> {
        let mut db = self.db.lock().unwrap();
        Ok(db.jobs.pop())
    }

    async fn settle(&self, _claim: ClaimedJob, _finish: JobFinish) -> StorageResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reusable contract functions
// ---------------------------------------------------------------------------

async fn transaction_carries_scope<S: RelationalStore>(rel: &S, auth: &AuthorizedScope) {
    let tx = rel.begin(auth.clone()).await.unwrap();
    assert_eq!(tx.scope(), auth.scope);
}

async fn idempotency_caches_replay<S: RelationalStore, Q: JobQueue>(
    rel: &S,
    queue: &Q,
    auth: &AuthorizedScope,
) {
    let mut tx = rel.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    assert!(
        tx.begin_command("memory.create", "contract-key", "hash-a")
            .await
            .unwrap()
            .is_none()
    );
    tx.enqueue(WorkItem {
        job_id: Uuid::new_v4(),
        kind: "publish".into(),
        payload: json!(null),
        asset: None,
        source: None,
    })
    .await
    .unwrap();
    tx.finish_command(json!({"asset_id": "cached"}))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // A replay with the same hash returns the cached response, no side effects.
    let mut replay = rel.begin(auth.clone()).await.unwrap();
    replay.check_permission(Permission::Write).await.unwrap();
    assert_eq!(
        replay
            .begin_command("memory.create", "contract-key", "hash-a")
            .await
            .unwrap(),
        Some(json!({"asset_id": "cached"}))
    );
    replay.rollback().await.unwrap();

    // The same key with a different hash is a conflict.
    let mut clash = rel.begin(auth.clone()).await.unwrap();
    clash.check_permission(Permission::Write).await.unwrap();
    assert!(matches!(
        clash
            .begin_command("memory.create", "contract-key", "hash-b")
            .await,
        Err(StorageError::Conflict(_))
    ));
    clash.rollback().await.unwrap();

    // exactly one job was enqueued, not two
    assert!(queue.claim_next().await.unwrap().is_some());
    assert!(queue.claim_next().await.unwrap().is_none());
}

async fn rollback_discards_business_and_enqueue<S: RelationalStore, Q: JobQueue>(
    rel: &S,
    queue: &Q,
    auth: &AuthorizedScope,
) {
    let mut tx = rel.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx.begin_command("memory.create", "rollback-key", "hash")
        .await
        .unwrap();
    tx.enqueue(WorkItem {
        job_id: Uuid::new_v4(),
        kind: "publish".into(),
        payload: json!(null),
        asset: None,
        source: None,
    })
    .await
    .unwrap();
    tx.rollback().await.unwrap();

    assert!(queue.claim_next().await.unwrap().is_none());
}

async fn permission_rechecked_at_commit<S: RelationalStore>(
    rel: &S,
    auth: &AuthorizedScope,
    revoke: impl FnOnce(),
) {
    let mut tx = rel.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    revoke();
    assert!(matches!(tx.commit().await, Err(StorageError::Forbidden)));
}

async fn queue_claim_settle_roundtrip<S: RelationalStore, Q: JobQueue>(
    rel: &S,
    queue: &Q,
    auth: &AuthorizedScope,
) {
    let job_id = Uuid::new_v4();
    let mut tx = rel.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx.begin_command("memory.create", "queue-key", "hash")
        .await
        .unwrap();
    tx.enqueue(WorkItem {
        job_id,
        kind: "publish".into(),
        payload: json!(null),
        asset: None,
        source: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let claimed = queue.claim_next().await.unwrap().unwrap();
    assert_eq!(claimed.job_id, job_id);
    queue.settle(claimed, JobFinish::Completed).await.unwrap();
    assert!(queue.claim_next().await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn setup() -> (MemRelationalStore, MemQueue, Arc<Mutex<MemDb>>) {
    let db = Arc::new(Mutex::new(MemDb::default()));
    (
        MemRelationalStore { db: db.clone() },
        MemQueue { db: db.clone() },
        db,
    )
}

fn scope() -> Scope {
    Scope {
        tenant_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
    }
}

fn seed(db: &Arc<Mutex<MemDb>>, scope: Scope, role: &str) -> AuthorizedScope {
    let principal = Uuid::new_v4();
    db.lock().unwrap().roles.insert(principal, role.to_string());
    AuthorizedScope {
        scope,
        principal_id: principal,
        role: role.to_string(),
    }
}

#[tokio::test]
async fn tx_carries_scope() {
    let (rel, _queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    transaction_carries_scope(&rel, &auth).await;
}

#[tokio::test]
async fn idempotent_command_caches_replay() {
    let (rel, queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    idempotency_caches_replay(&rel, &queue, &auth).await;
}

#[tokio::test]
async fn rollback_discards_enqueue() {
    let (rel, queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    rollback_discards_business_and_enqueue(&rel, &queue, &auth).await;
}

#[tokio::test]
async fn commit_records_source_audit_and_enqueue() {
    let (rel, queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    let source = SourceVersion {
        source_id: Uuid::new_v4(),
        version: 1,
    };

    let mut tx = rel.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx.begin_command("knowledge.capture", "capture-key", "hash")
        .await
        .unwrap();
    tx.record_source(source).await.unwrap();
    tx.audit("capture", Uuid::new_v4(), json!({"chunks": 1}))
        .await
        .unwrap();
    tx.enqueue(WorkItem {
        job_id: Uuid::new_v4(),
        kind: "publish".into(),
        payload: json!(null),
        asset: None,
        source: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();

    {
        let db = db.lock().unwrap();
        assert!(db.sources.contains(&source));
        assert_eq!(db.audits.len(), 1);
    }
    assert!(queue.claim_next().await.unwrap().is_some());
}

#[tokio::test]
async fn permission_rechecked_at_commit_blocks_revoked_principal() {
    let (rel, _queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    let principal = auth.principal_id;
    permission_rechecked_at_commit(&rel, &auth, move || {
        db.lock().unwrap().roles.insert(principal, "reader".into());
    })
    .await;
}

#[tokio::test]
async fn queue_claim_settle_roundtrip_works() {
    let (rel, queue, db) = setup();
    let auth = seed(&db, scope(), "writer");
    queue_claim_settle_roundtrip(&rel, &queue, &auth).await;
}
