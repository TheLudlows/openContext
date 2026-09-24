//! Ledger reconcile tests (M3): stuck pending and due retry_wait converge.

use opencontext::storage::ledger::{LedgerEntry, LedgerKey, Surface};
use opencontext::storage::{
    AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion,
    sqlite::SqliteStore,
};

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

#[tokio::test]
async fn reconcile_orphans_stuck_pending_and_requeues_due_retry_wait() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    let stuck = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let due = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(stuck.clone()).await.unwrap();
        tx.register_pending(due.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Make `stuck` look abandoned and `due` already past its retry time.
    sqlx::query(
        "UPDATE oc_artifact_ledger SET state = 'retry_wait', next_retry_at = 0, attempt = 1 \
         WHERE artifact_id = ?",
    )
    .bind(due.key.artifact_id)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE oc_artifact_ledger SET updated_at = 0 WHERE artifact_id = ?")
        .bind(stuck.key.artifact_id)
        .execute(store.pool())
        .await
        .unwrap();

    let report = store.reconcile_ledger().await.unwrap();
    assert_eq!(report["orphaned"].as_i64().unwrap(), 1);
    assert_eq!(report["requeued"].as_i64().unwrap(), 1);

    let stuck_state: String =
        sqlx::query_scalar("SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?")
            .bind(stuck.key.artifact_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(stuck_state, "orphan");

    let due_state: String =
        sqlx::query_scalar("SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?")
            .bind(due.key.artifact_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(due_state, "pending");
}

#[tokio::test]
async fn confirm_is_idempotent_and_republishes_nothing_twice() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    // A live source so confirm's source re-check passes (A2.6 step 3).
    let source_id = {
        let mut tx = begin_write(&store, &auth).await;
        let id = tx.create_event("memory", "hello", None).await.unwrap();
        tx.commit().await.unwrap();
        id
    };
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Confirm twice: the second is a no-op, not an error.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }

    let state: String =
        sqlx::query_scalar("SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?")
            .bind(e.key.artifact_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(state, "committed");
}
