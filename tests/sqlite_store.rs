//! SQLite adapter tests (M2): privileged ops, scoped transactions, idempotency
//! and the self-polled queue against a real database file in a temp dir.

use opencontext::storage::{
    AuthorizedScope, DomainTx, IssuedKey, JobFinish, JobQueue, Permission, RelationalStore, Scope,
    StorageError, WorkItem, sqlite::SqliteStore,
};
use serde_json::json;
use uuid::Uuid;

struct Provisioned {
    scope: Scope,
    key: IssuedKey,
    auth: AuthorizedScope,
}

async fn provision(store: &SqliteStore, name: &str) -> Provisioned {
    let tenant = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    store
        .create_workspace(tenant, workspace, name)
        .await
        .unwrap();
    let scope = Scope {
        tenant_id: tenant,
        workspace_id: workspace,
    };
    let key = store.issue_key(scope, "admin").await.unwrap();
    let auth = store.authenticate(&key.token).await.unwrap();
    Provisioned { scope, key, auth }
}

#[tokio::test]
async fn key_roundtrip_and_revoke() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();

    let p = provision(&store, "acme").await;
    assert_eq!(p.auth.scope, p.scope);
    assert_eq!(p.auth.role, "admin");
    assert_eq!(p.auth.principal_id, p.key.key_id);

    store.revoke_key(p.key.key_id).await.unwrap();
    assert!(matches!(
        store.authenticate(&p.key.token).await,
        Err(StorageError::Forbidden)
    ));

    // Unknown roles are rejected at issue time.
    assert!(matches!(
        store.issue_key(p.scope, "superuser").await,
        Err(StorageError::Conflict(_))
    ));
}

#[tokio::test]
async fn transaction_commits_enqueue_audit_and_command() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;
    let job_id = Uuid::new_v4();

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        assert!(
            tx.begin_command("memory.create", "k1", "h1")
                .await
                .unwrap()
                .is_none()
        );
        tx.enqueue(WorkItem {
            job_id,
            kind: "publish".into(),
            payload: json!({"a": 1}),
            asset: None,
            source: None,
        })
        .await
        .unwrap();
        tx.audit("memory.create", Uuid::new_v4(), json!({"a": 1}))
            .await
            .unwrap();
        tx.finish_command(json!({"asset_id": "x"})).await.unwrap();
        tx.commit().await.unwrap();
    }

    let claimed = store.claim_next().await.unwrap().unwrap();
    assert_eq!(claimed.job_id, job_id);
    assert_eq!(claimed.scope, p.scope);
    assert_eq!(claimed.kind, "publish");
    assert_eq!(claimed.generation, 1);
    assert_eq!(claimed.run_token, 1);
    store.settle(claimed, JobFinish::Completed).await.unwrap();
    assert!(store.claim_next().await.unwrap().is_none());

    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM oc_audit")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn idempotent_replay_caches_response() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        assert!(
            tx.begin_command("memory.create", "k1", "h1")
                .await
                .unwrap()
                .is_none()
        );
        tx.finish_command(json!({"asset_id": "cached"}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Same key + same hash returns the cached response.
    let mut replay = store.begin(p.auth.clone()).await.unwrap();
    replay.check_permission(Permission::Write).await.unwrap();
    assert_eq!(
        replay
            .begin_command("memory.create", "k1", "h1")
            .await
            .unwrap(),
        Some(json!({"asset_id": "cached"}))
    );
    replay.rollback().await.unwrap();

    // Same key + different hash is a conflict.
    let mut clash = store.begin(p.auth.clone()).await.unwrap();
    clash.check_permission(Permission::Write).await.unwrap();
    assert!(matches!(
        clash.begin_command("memory.create", "k1", "h2").await,
        Err(StorageError::Conflict(_))
    ));
    clash.rollback().await.unwrap();
}

#[tokio::test]
async fn rollback_discards_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.begin_command("memory.create", "k1", "h1").await.unwrap();
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
    }

    assert!(store.claim_next().await.unwrap().is_none());
    let command_count: i64 = sqlx::query_scalar("SELECT count(*) FROM oc_commands")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(command_count, 0);
}

#[tokio::test]
async fn permission_rechecked_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    store.revoke_key(p.key.key_id).await.unwrap();
    assert!(matches!(tx.commit().await, Err(StorageError::Forbidden)));
}

#[tokio::test]
async fn cross_tenant_idempotency_is_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let a = provision(&store, "tenant-a").await;
    let b = provision(&store, "tenant-b").await;

    // The same key + hash is a first execution in each tenant.
    for p in [&a, &b] {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        assert!(
            tx.begin_command("memory.create", "same-key", "h")
                .await
                .unwrap()
                .is_none()
        );
        tx.finish_command(json!({"owner": p.scope.workspace_id}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Replay in tenant A returns A's cached response, not B's.
    let mut replay = store.begin(a.auth.clone()).await.unwrap();
    replay.check_permission(Permission::Write).await.unwrap();
    assert_eq!(
        replay
            .begin_command("memory.create", "same-key", "h")
            .await
            .unwrap(),
        Some(json!({"owner": a.scope.workspace_id}))
    );
    replay.rollback().await.unwrap();
}

#[tokio::test]
async fn retry_wait_defers_claim_until_due() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;
    let job_id = Uuid::new_v4();

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.begin_command("capture", "k", "h").await.unwrap();
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
    }

    let claimed = store.claim_next().await.unwrap().unwrap();
    store
        .settle(claimed, JobFinish::Retryable { attempt: 1 })
        .await
        .unwrap();

    // Not claimable while next_retry_at is in the future.
    assert!(store.claim_next().await.unwrap().is_none());

    // Once due, the same job is claimed again with a bumped run_token.
    sqlx::query("UPDATE oc_jobs SET next_retry_at = 0")
        .execute(store.pool())
        .await
        .unwrap();
    let retried = store.claim_next().await.unwrap().unwrap();
    assert_eq!(retried.job_id, job_id);
    assert_eq!(retried.run_token, 2);
    store.settle(retried, JobFinish::Completed).await.unwrap();
    assert!(store.claim_next().await.unwrap().is_none());
}

#[tokio::test]
async fn abandoned_processing_job_is_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;
    let job_id = Uuid::new_v4();

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.begin_command("capture", "k", "h").await.unwrap();
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
    }

    // Simulate a worker that claimed the job then crashed without settling.
    sqlx::query("UPDATE oc_jobs SET state = 'processing', run_token = 1, updated_at = 0")
        .execute(store.pool())
        .await
        .unwrap();

    let recovered = store.claim_next().await.unwrap().unwrap();
    assert_eq!(recovered.job_id, job_id);
    assert_eq!(recovered.run_token, 2);
    store.settle(recovered, JobFinish::Completed).await.unwrap();
}
