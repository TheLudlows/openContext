//! Ledger type unit tests (M3): surface/state encoding and deterministic keys.

use opencontext::storage::ledger::{
    LedgerEntry, LedgerKey, LedgerState, MAX_LEDGER_ATTEMPTS, Surface, ledger_idempotency_key,
};
use opencontext::storage::{
    AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion, StorageError,
    sqlite::SqliteStore,
};
use uuid::Uuid;

fn key() -> LedgerKey {
    LedgerKey {
        scope: Scope {
            tenant_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        },
        source: SourceVersion {
            source_id: Uuid::new_v4(),
            version: 1,
        },
        artifact_id: Uuid::new_v4(),
        surface: Surface::Graph,
        generation: 2,
    }
}

#[test]
fn surface_and_state_encode_to_schema_tokens() {
    assert_eq!(Surface::Vector.as_str(), "vector");
    assert_eq!(Surface::Graph.as_str(), "graph");
    assert_eq!(LedgerState::Pending.as_str(), "pending");
    assert_eq!(LedgerState::Committed.as_str(), "committed");
    assert_eq!(LedgerState::RetryWait.as_str(), "retry_wait");
    assert_eq!(LedgerState::Orphan.as_str(), "orphan");
}

#[test]
fn idempotency_key_is_deterministic_and_scoped() {
    let k = key();
    let a = LedgerEntry {
        key: k,
        artifact_type: "entity".into(),
        idempotency_key: ledger_idempotency_key(&k),
    };
    let b = LedgerEntry {
        key: k,
        artifact_type: "entity".into(),
        idempotency_key: ledger_idempotency_key(&k),
    };
    assert_eq!(a.idempotency_key, b.idempotency_key);

    let mut other = k;
    other.generation = 3;
    assert_ne!(ledger_idempotency_key(&k), ledger_idempotency_key(&other));
}

async fn provision(store: &SqliteStore) -> AuthorizedScope {
    let tenant = uuid::Uuid::new_v4();
    let workspace = uuid::Uuid::new_v4();
    store
        .create_workspace(tenant, workspace, "acme")
        .await
        .unwrap();
    let scope = Scope {
        tenant_id: tenant,
        workspace_id: workspace,
    };
    let key = store.issue_key(scope, "admin").await.unwrap();
    store.authenticate(&key.token).await.unwrap()
}

async fn begin_write(
    store: &SqliteStore,
    auth: &AuthorizedScope,
) -> opencontext::storage::sqlite::SqliteTx {
    let mut tx = store.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx
}

/// Create a live `active` source event and return its id, so `confirm_committed`
/// has a valid source to re-check (A2.6 step 3).
async fn create_active_event(store: &SqliteStore, auth: &AuthorizedScope) -> uuid::Uuid {
    let mut tx = begin_write(store, auth).await;
    let id = tx.create_event("memory", "hello", None).await.unwrap();
    tx.commit().await.unwrap();
    id
}

fn entry(scope: Scope, source_id: uuid::Uuid, artifact_id: uuid::Uuid) -> LedgerEntry {
    LedgerEntry {
        key: LedgerKey {
            scope,
            source: SourceVersion {
                source_id,
                version: 1,
            },
            artifact_id,
            surface: Surface::Vector,
            generation: 1,
        },
        artifact_type: "chunk".into(),
        idempotency_key: "k".into(),
    }
}

async fn ledger_state(store: &SqliteStore, key: &LedgerKey) -> String {
    sqlx::query_scalar(
        "SELECT state FROM oc_artifact_ledger \
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
    .fetch_one(store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn pending_confirms_committed_when_source_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    let source_id = create_active_event(&store, &auth).await;
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "pending");

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "committed");
}

#[tokio::test]
async fn confirm_is_a_noop_when_source_is_retracted() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    let source_id = create_active_event(&store, &auth).await;
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Retract the source, then confirm must NOT publish.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.retract_event(source_id).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "pending");
}

#[tokio::test]
async fn retryable_then_permanent_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    // fail_retryable/fail_permanent do not re-check the source, so a random
    // (non-existent) source id is fine here.
    let e = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // First failure is retryable.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.fail_retryable(e.key, "boom").await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "retry_wait");

    // Exhaust the budget to orphan.
    for _ in 0..MAX_LEDGER_ATTEMPTS {
        let mut tx = begin_write(&store, &auth).await;
        tx.fail_retryable(e.key, "boom").await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "orphan");
}

#[tokio::test]
async fn ledger_methods_reject_a_mismatched_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    let foreign = Scope {
        tenant_id: uuid::Uuid::new_v4(),
        workspace_id: uuid::Uuid::new_v4(),
    };
    let e = entry(foreign, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        assert!(matches!(
            tx.register_pending(e.clone()).await,
            Err(StorageError::Forbidden)
        ));
        assert!(matches!(
            tx.confirm_committed(e.key).await,
            Err(StorageError::Forbidden)
        ));
        assert!(matches!(
            tx.fail_retryable(e.key, "boom").await,
            Err(StorageError::Forbidden)
        ));
        assert!(matches!(
            tx.fail_permanent(e.key, "boom").await,
            Err(StorageError::Forbidden)
        ));
    }
}
