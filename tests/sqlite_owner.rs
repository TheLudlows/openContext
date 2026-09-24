//! Provenance owner tests (M3): register, shared owners, detach, ownership.

use opencontext::storage::{
    AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion,
    sqlite::SqliteStore,
};
use uuid::Uuid;

async fn provision(store: &SqliteStore) -> AuthorizedScope {
    let tenant = Uuid::new_v4();
    let workspace = Uuid::new_v4();
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

#[tokio::test]
async fn detaching_a_source_drops_its_sole_artifacts_but_retains_shared() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    let s1 = SourceVersion {
        source_id: Uuid::new_v4(),
        version: 1,
    };
    let s2 = SourceVersion {
        source_id: Uuid::new_v4(),
        version: 1,
    };
    let shared = Uuid::new_v4();
    let sole = Uuid::new_v4();

    // s1 and s2 both own `shared`; only s1 owns `sole`.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_owner(s1, "entity", shared, None).await.unwrap();
        tx.register_owner(s1, "entity", sole, None).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_owner(s2, "entity", shared, None).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Detaching s1 removes exactly its two owner rows. (The write must be
    // committed: dropping a `SqliteTx` without `commit` rolls it back.)
    let removed = {
        let mut tx = begin_write(&store, &auth).await;
        let removed = tx.detach_owners(s1).await.unwrap();
        tx.commit().await.unwrap();
        removed
    };
    assert_eq!(removed, 2);

    // `shared` is still owned (by s2) and stays; `sole` has no owner left and
    // is gone from the owned set (M4 will diff this against Kuzu to reap it).
    let owned = {
        let mut tx = begin_write(&store, &auth).await;
        tx.owned_artifacts().await.unwrap()
    };
    let entities: Vec<String> = owned["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(entities.contains(&shared.to_string()));
    assert!(!entities.contains(&sole.to_string()));
}
