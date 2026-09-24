//! Single-worker OS file lock (M2, A2.5): the lock is bound to the database
//! path, reentrant within one process (API and Worker share an engine), and
//! exclusive across processes (a second worker is rejected).

use std::path::Path;

use opencontext::storage::{StorageError, sqlite::SqliteStore};

#[tokio::test]
async fn reopen_in_process_is_reentrant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oc.db");

    let _first = SqliteStore::open(&path).await.unwrap();
    // A second open in the same process shares the existing holder instead of
    // deadlocking the process against its own OS lock.
    let _second = SqliteStore::open(&path).await.unwrap();
}

#[tokio::test]
async fn lock_is_exclusive_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oc.db");
    let ready = dir.path().join("ready");

    // Spawn a helper that opens the store and signals once it holds the lock.
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args(["--exact", "lock_helper_holds_store", "--nocapture"])
        .env("OC_LOCK_HELPER_PATH", &path)
        .env("OC_LOCK_HELPER_READY", &ready)
        .spawn()
        .unwrap();

    // Wait (bounded) for the helper to report it has the lock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("helper did not acquire the lock in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // A different process opening the same path must be rejected.
    let err = match SqliteStore::open(&path).await {
        Err(e) => e,
        Ok(_) => panic!("expected the second process to be rejected"),
    };
    assert!(
        matches!(err, StorageError::Conflict(_)),
        "expected a lock conflict, got {err:?}"
    );

    // When the holder exits, the OS releases the lock and a fresh open wins.
    let _ = child.kill();
    let _ = child.wait();
    assert!(SqliteStore::open(&path).await.is_ok());
}

/// Helper process entry point: open the store, signal readiness, then hold the
/// lock until the parent kills us. No-op when run without the env vars.
#[tokio::test]
async fn lock_helper_holds_store() {
    let (Some(path), Some(ready)) = (
        std::env::var_os("OC_LOCK_HELPER_PATH"),
        std::env::var_os("OC_LOCK_HELPER_READY"),
    ) else {
        return;
    };
    let _store = SqliteStore::open(Path::new(&path)).await.unwrap();
    std::fs::write(&ready, "ok").unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
}
