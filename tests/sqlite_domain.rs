//! SQLite domain-operation tests (M2): the A5 objects behind [`DomainTx`] —
//! events, memory/knowledge assets, candidates, reviews, versions, chunks,
//! summaries, files and jobs — against a real database file in a temp dir.

use opencontext::storage::{
    AuthorizedScope, DomainTx, IssuedKey, Permission, RelationalStore, Scope, StorageError,
    WorkItem, sqlite::SqliteStore,
};
use serde_json::json;
use uuid::Uuid;

struct Provisioned {
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
    let key: IssuedKey = store.issue_key(scope, "admin").await.unwrap();
    let auth = store.authenticate(&key.token).await.unwrap();
    Provisioned { auth }
}

/// memory → candidate → version → chunk/summary → published asset, all in one
/// transaction, then read back through the views.
#[tokio::test]
async fn memory_lifecycle_publishes_an_asset() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let content = "生产发布需要审批";
    let asset;
    let source;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        source = tx.create_event("structured", content, None).await.unwrap();
        let (a, version) = tx.slot("fact.release").await.unwrap();
        assert!(version.is_none());
        let candidate = tx
            .insert_candidate(a, "fact.release", content, source, true, None)
            .await
            .unwrap();
        tx.insert_version(a, 1, content, "hash-1", source, None, None)
            .await
            .unwrap();
        let chunk = Uuid::new_v4();
        tx.insert_chunk(
            chunk,
            a,
            1,
            0,
            content,
            &json!({"byte_start": 0}),
            "审批 生产",
        )
        .await
        .unwrap();
        tx.insert_summary(Uuid::new_v4(), chunk, "summary", "model-v1", "审批")
            .await
            .unwrap();
        tx.update_asset_version(a, 1, None).await.unwrap();
        asset = a;
        let _ = candidate;
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Read).await.unwrap();
    let view = tx.asset_view(asset, None).await.unwrap();
    assert_eq!(view["content"], content);
    assert_eq!(view["version"], 1);
    assert_eq!(view["source_event_id"], source.to_string());
    tx.rollback().await.unwrap();
}

/// Review validates the candidate against its asset/source, then records a
/// review and moves the candidate state.
#[tokio::test]
async fn review_approves_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let source;
    let candidate;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        source = tx
            .create_event("structured", "content", None)
            .await
            .unwrap();
        let (asset, _) = tx.slot("fact.x").await.unwrap();
        candidate = tx
            .insert_candidate(asset, "fact.x", "content", source, false, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let review = Uuid::new_v4();
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Review).await.unwrap();
        let row = tx.candidate_for_review(candidate).await.unwrap();
        assert_eq!(row["state"], "candidate");
        assert_eq!(row["source_state"], "active");
        assert_eq!(row["deleted"], false);
        tx.apply_review(review, candidate, 1, "approve", None, "looks good")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Review).await.unwrap();
    let view = tx.candidate_view(candidate).await.unwrap();
    assert_eq!(view["state"], "approved");
    tx.rollback().await.unwrap();
}

/// A knowledge asset's current version feeds restore_source.
#[tokio::test]
async fn knowledge_asset_and_restore_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let asset;
    let source;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        asset = tx.insert_knowledge_asset("Handbook").await.unwrap();
        source = tx.create_event("knowledge", "text", None).await.unwrap();
        tx.insert_version(asset, 1, "text", "hash", source, None, None)
            .await
            .unwrap();
        tx.update_asset_version(asset, 1, None).await.unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Review).await.unwrap();
    let row = tx.restore_source(asset, 1).await.unwrap();
    assert_eq!(row["current_version"], 1);
    assert_eq!(row["source_event_id"], source.to_string());
    tx.rollback().await.unwrap();
}

/// Files carry metadata and are soft-deleted; the hash survives until then.
#[tokio::test]
async fn file_metadata_and_soft_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let file;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        file = tx
            .insert_file("a.txt", "text", "hash-abc", 3)
            .await
            .unwrap();
        assert!(tx.file_exists(file).await.unwrap());
        assert_eq!(
            tx.file_hash(file).await.unwrap().as_deref(),
            Some("hash-abc")
        );
        tx.commit().await.unwrap();
    }

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Delete).await.unwrap();
        assert_eq!(tx.delete_file(file).await.unwrap(), 1);
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    assert!(!tx.file_visible(file).await.unwrap());
    tx.rollback().await.unwrap();
}

/// Enqueue → job_row → retry bumps generation → settle completed → job_view.
#[tokio::test]
async fn job_action_and_settle() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let job = Uuid::new_v4();
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.enqueue(WorkItem {
            job_id: job,
            kind: "publish".into(),
            payload: json!({"expected_version": null}),
            asset: None,
            source: None,
        })
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    // The enqueued job is visible and owned by the principal.
    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    let row = tx.job_row(job).await.unwrap();
    assert_eq!(row["state"], "pending");
    assert_eq!(row["created_by"], p.auth.principal_id.to_string());
    assert_eq!(tx.set_job_action(job, "retry").await.unwrap(), 2);
    tx.commit().await.unwrap();

    // Settle it as completed under the bumped generation/run_token.
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.settle_completed(job, "published", &json!({"asset_id": "a"}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    let view = tx.job_view(job).await.unwrap();
    assert_eq!(view["state"], "completed");
    assert_eq!(view["generation"], 2);
    tx.rollback().await.unwrap();
}

/// valid_targets rejects a deleted asset or a retracted source.
#[tokio::test]
async fn valid_targets_rejects_deleted_or_retracted() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let asset;
    let source;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        asset = tx.insert_knowledge_asset("A").await.unwrap();
        source = tx.create_event("knowledge", "x", None).await.unwrap();
        tx.commit().await.unwrap();
    }

    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        tx.valid_targets(Some(asset), Some(source)).await.unwrap();
        tx.retract_event(source).await.unwrap();
        assert!(matches!(
            tx.valid_targets(None, Some(source)).await,
            Err(StorageError::Conflict(_))
        ));
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx.delete_asset(asset).await.unwrap();
    assert!(matches!(
        tx.valid_targets(Some(asset), None).await,
        Err(StorageError::Conflict(_))
    ));
    tx.rollback().await.unwrap();
}

/// candidates_view returns only open candidates across a workspace.
#[tokio::test]
async fn candidates_view_lists_open_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let p = provision(&store, "acme").await;

    let source;
    let open;
    {
        let mut tx = store.begin(p.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        source = tx
            .create_event("structured", "content", None)
            .await
            .unwrap();
        let (asset, _) = tx.slot("fact.c").await.unwrap();
        open = tx
            .insert_candidate(asset, "fact.c", "content", source, false, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = store.begin(p.auth.clone()).await.unwrap();
    tx.check_permission(Permission::Review).await.unwrap();
    let view = tx.candidates_view().await.unwrap();
    let items = view["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], open.to_string());
    assert_eq!(items[0]["state"], "candidate");
    tx.rollback().await.unwrap();
}

/// Fact slots are scoped: the same key resolves to different assets per tenant.
#[tokio::test]
async fn fact_slots_are_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let a = provision(&store, "tenant-a").await;
    let b = provision(&store, "tenant-b").await;

    let (asset_a, asset_b);
    {
        let mut tx = store.begin(a.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        asset_a = tx.slot("fact.shared").await.unwrap().0;
        tx.commit().await.unwrap();
    }
    {
        let mut tx = store.begin(b.auth.clone()).await.unwrap();
        tx.check_permission(Permission::Write).await.unwrap();
        asset_b = tx.slot("fact.shared").await.unwrap().0;
        tx.commit().await.unwrap();
    }
    assert_ne!(asset_a, asset_b);
}
