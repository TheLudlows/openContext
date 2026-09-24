use opencontext::db;
use opencontext::storage::{Lifecycle, sqlite::SqliteStore};
use sqlx::{ConnectOptions, Connection, Executor, PgConnection, postgres::PgConnectOptions};
use std::str::FromStr;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires isolated TEST_ADMIN_DATABASE_URL with CREATE DATABASE permission"]
async fn empty_database_initializes_once_and_rejects_partial_schema() -> anyhow::Result<()> {
    let url = std::env::var("TEST_ADMIN_DATABASE_URL")?;
    let options = PgConnectOptions::from_str(&url)?;
    let mut admin = PgConnection::connect_with(&options).await?;
    // Only this disposable database is ever changed or dropped.
    let name = format!("oc_init_test_{}", Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await?;
    let result: anyhow::Result<()> = async {
        let url = options.clone().database(&name).to_url_lossy().to_string();
        let (first, second) = tokio::join!(db::connect(&url), db::connect(&url));
        let pool = first?;
        second?.close().await;
        let history: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations')::text")
                .fetch_one(&pool)
                .await?;
        assert!(history.is_none());
        let workspace = db::provision(&pool, "initialization sentinel").await?;
        db::initialize(&pool).await?;
        let auth = db::authenticate(&pool, workspace["token"].as_str().unwrap()).await?;
        assert_eq!(
            auth.workspace_id.to_string(),
            workspace["workspace_id"].as_str().unwrap()
        );
        // The fixed queue schema must support the enqueue function used by Service.
        let queue_id: String =
            sqlx::query_scalar("SELECT id FROM apalis.push_job('initialization-test','{}'::json)")
                .fetch_one(&pool)
                .await?;
        assert_eq!(queue_id.len(), 26);
        // Do not silently repair an existing database or erase its contents.
        sqlx::query("ALTER TABLE oc.versions DROP COLUMN title")
            .execute(&pool)
            .await?;
        assert!(db::initialize(&pool).await.is_err());
        assert!(
            db::authenticate(&pool, workspace["token"].as_str().unwrap())
                .await
                .is_ok()
        );
        pool.close().await;
        Ok(())
    }
    .await;
    // Also clean up if an initialization operation returned an error.
    admin
        .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
        .await?;
    result
}

#[tokio::test]
async fn sqlite_initializes_once_and_rejects_incompatible() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("oc.db");

    // A fresh file is created with the complete schema.
    let store = SqliteStore::open(&path).await?;
    store.check().await?;

    // Reopening the same file is a no-op: initialize and check stay idempotent.
    let again = SqliteStore::open(&path).await?;
    again.initialize().await?;
    again.check().await?;

    // A missing table is reported as incompatible, never silently repaired.
    sqlx::query("DROP TABLE oc_audit")
        .execute(store.pool())
        .await?;
    assert!(store.check().await.is_err());

    Ok(())
}

#[tokio::test]
async fn sqlite_concurrent_initialization_is_serialized() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("oc.db");
    let (left, right) = tokio::join!(SqliteStore::open(&path), SqliteStore::open(&path));
    left?.check().await?;
    right?.check().await?;
    Ok(())
}
