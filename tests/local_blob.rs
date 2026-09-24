//! Local blob store tests (M2): scope partitioning, put/get/delete and the
//! path-escape guard.

use opencontext::storage::{
    BlobKey, BlobStore, Lifecycle, Scope, StorageError, local_blob::LocalBlobStore,
};
use uuid::Uuid;

fn scope() -> Scope {
    Scope {
        tenant_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
    }
}

fn key(scope: Scope, root: &str, path: &str) -> BlobKey {
    BlobKey {
        scope,
        root: root.to_string(),
        path: path.to_string(),
    }
}

#[tokio::test]
async fn put_get_delete_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::open(dir.path().join("blobs"))
        .await
        .unwrap();
    let k = key(scope(), "uploads", "a.txt");

    store.put(&k, b"hello".to_vec()).await.unwrap();
    assert_eq!(store.get(&k).await.unwrap(), b"hello");
    store.delete(&k).await.unwrap();
    assert!(matches!(store.get(&k).await, Err(StorageError::NotFound)));
}

#[tokio::test]
async fn delete_missing_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::open(dir.path().join("blobs"))
        .await
        .unwrap();
    store
        .delete(&key(scope(), "uploads", "missing.txt"))
        .await
        .unwrap();
}

#[tokio::test]
async fn scope_partitions_paths() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::open(dir.path().join("blobs"))
        .await
        .unwrap();
    let a = key(scope(), "uploads", "x");
    let b = key(scope(), "uploads", "x");
    store.put(&a, b"a".to_vec()).await.unwrap();
    store.put(&b, b"b".to_vec()).await.unwrap();
    assert_eq!(store.get(&a).await.unwrap(), b"a");
    assert_eq!(store.get(&b).await.unwrap(), b"b");
}

#[tokio::test]
async fn path_escape_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalBlobStore::open(dir.path().join("blobs"))
        .await
        .unwrap();

    let parent = key(scope(), "uploads", "../../escape.txt");
    assert!(store.put(&parent, b"x".to_vec()).await.is_err());

    let absolute = BlobKey {
        scope: scope(),
        root: "uploads".to_string(),
        path: "/etc/passwd".to_string(),
    };
    assert!(store.put(&absolute, b"x".to_vec()).await.is_err());
}

#[tokio::test]
async fn check_detects_missing_base() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("blobs");
    let store = LocalBlobStore::open(&base).await.unwrap();
    store.check().await.unwrap();
    std::fs::remove_dir_all(&base).unwrap();
    assert!(store.check().await.is_err());
}
