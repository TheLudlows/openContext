use crate::{
    error::{AppError, Result},
    types::AuthContext,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

pub type Tx = Transaction<'static, Postgres>;
pub const QUEUE: &str = "opencontext-v1";
pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?;
    initialize(&pool).await?;
    Ok(pool)
}

pub async fn check_runtime(pool: &PgPool) -> anyhow::Result<()> {
    let unsafe_role: bool = sqlx::query_scalar("SELECT r.rolsuper OR r.rolbypassrls OR EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('oc','apalis') AND pg_has_role(current_user,c.relowner,'MEMBER')) FROM pg_roles r WHERE r.rolname=current_user").fetch_one(pool).await?;
    anyhow::ensure!(
        !unsafe_role,
        "runtime database role must not be superuser, BYPASSRLS, or a schema table owner/member"
    );
    Ok(())
}

/// Bootstrap an empty database once. Existing schemas are never upgraded or repaired.
pub async fn initialize(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    // API, worker and administrative processes can start concurrently.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('opencontext:initialize',0))")
        .execute(&mut *tx)
        .await?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname IN ('oc','apalis'))",
    )
    .fetch_one(&mut *tx)
    .await?;
    if !existing {
        // PostgreSQL needs a bootstrap administrator; runtime roles remain non-owners.
        sqlx::raw_sql(include_str!("../deploy/postgres-schema.sql"))
            .execute(&mut *tx)
            .await?;
        sqlx::raw_sql(include_str!("../deploy/postgres-queue.sql"))
            .execute(&mut *tx)
            .await?;
    }
    let compatible: bool = sqlx::query_scalar(include_str!("../deploy/postgres-check.sql"))
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        compatible,
        "incomplete or incompatible database schema; automatic initialization only creates an empty database and never upgrades existing data"
    );
    tx.commit().await?;
    Ok(())
}

pub async fn configure_runtime(pool: &PgPool, password: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        password.len() >= 24,
        "runtime password must contain at least 24 characters"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('oc.bootstrap_password',$1,true)")
        .bind(password)
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='oc_runtime') THEN CREATE ROLE oc_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; END IF; EXECUTE format('ALTER ROLE oc_runtime PASSWORD %L',current_setting('oc.bootstrap_password')); END $$;")
        .execute(&mut *tx).await?;
    sqlx::raw_sql(include_str!("../deploy/runtime-grants.sql"))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn authenticate(pool: &PgPool, token: &str) -> Result<AuthContext> {
    if token.len() < 32 || token.len() > 256 {
        return Err(AppError::Unauthorized);
    }
    sqlx::query_as("SELECT * FROM oc.authenticate($1)")
        .bind(hash(token))
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::Unauthorized)
}

pub async fn scoped(pool: &PgPool, tenant: Uuid, workspace: Uuid, write: bool) -> Result<Tx> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('oc.tenant_id',$1,true),set_config('oc.workspace_id',$2,true),set_config('statement_timeout','15000',true),set_config('lock_timeout','10000',true)")
        .bind(tenant.to_string()).bind(workspace.to_string()).execute(&mut *tx).await?;
    if write {
        // Serialize short mutation/commit sections within one workspace. No model/file IO under lock.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("{tenant}/{workspace}"))
            .execute(&mut *tx)
            .await?;
    }
    Ok(tx)
}

pub async fn authorized(
    pool: &PgPool,
    auth: &AuthContext,
    permission: &str,
    write: bool,
) -> Result<Tx> {
    let mut tx = scoped(pool, auth.tenant_id, auth.workspace_id, write).await?;
    require_current(&mut tx, auth, permission).await?;
    Ok(tx)
}

pub async fn require_current(tx: &mut Tx, auth: &AuthContext, permission: &str) -> Result<()> {
    let current: AuthContext = sqlx::query_as(
        "SELECT id,tenant_id,workspace_id,role FROM oc.api_keys WHERE id=$1 AND NOT revoked",
    )
    .bind(auth.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::Unauthorized)?;
    current.require(permission)
}

pub async fn audit(
    tx: &mut Tx,
    auth: &AuthContext,
    action: &str,
    target: Uuid,
    details: serde_json::Value,
) -> Result<()> {
    sqlx::query("INSERT INTO oc.audit(tenant_id,workspace_id,id,actor,action,target,details) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(auth.tenant_id).bind(auth.workspace_id).bind(Uuid::new_v4()).bind(auth.id).bind(action).bind(target).bind(details).execute(&mut **tx).await?;
    Ok(())
}

pub async fn provision(pool: &PgPool, name: &str) -> anyhow::Result<serde_json::Value> {
    let tenant = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    sqlx::query("INSERT INTO oc.workspaces(tenant_id,id,name) VALUES($1,$2,$3)")
        .bind(tenant)
        .bind(workspace)
        .bind(name)
        .execute(pool)
        .await?;
    issue_key(pool, workspace, "admin").await
}

pub async fn issue_key(
    pool: &PgPool,
    workspace: Uuid,
    role: &str,
) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(
        ["reader", "writer", "reviewer", "admin"].contains(&role),
        "invalid role"
    );
    let tenant: Uuid = sqlx::query_scalar("SELECT tenant_id FROM oc.workspaces WHERE id=$1")
        .bind(workspace)
        .fetch_one(pool)
        .await?;
    let token = format!("oc_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO oc.api_keys(id,tenant_id,workspace_id,token_hash,role) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(workspace)
    .bind(hash(&token))
    .bind(role)
    .execute(pool)
    .await?;
    Ok(
        serde_json::json!({"key_id":id,"tenant_id":tenant,"workspace_id":workspace,"role":role,"token":token}),
    )
}
