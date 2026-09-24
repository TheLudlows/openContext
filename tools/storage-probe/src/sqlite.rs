use crate::*;
use sqlx::{Connection, Executor, SqliteConnection, sqlite::*};

pub async fn run(path: &Path, request: Value) -> Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(750));
    let mut conn = SqliteConnection::connect_with(&options).await?;
    if request["op"] == "init" {
        conn.execute("CREATE TABLE IF NOT EXISTS records(tenant TEXT NOT NULL,workspace TEXT NOT NULL,id TEXT NOT NULL,value INTEGER NOT NULL,PRIMARY KEY(tenant,workspace,id))").await?;
        return emit(json!({"ok": true}));
    }
    let mut requests = Requests::new(request)?;
    while let Some(r) = requests.next()? {
        let op = field(&r, "op")?;
        let tenant = field(&r, "tenant")?;
        let workspace = field(&r, "workspace")?;
        let id = field(&r, "id")?;
        match op {
            "put" | "hold-write" => {
                if op == "hold-write" {
                    conn.execute("BEGIN IMMEDIATE").await?;
                }
                sqlx::query("INSERT INTO records VALUES(?,?,?,?) ON CONFLICT(tenant,workspace,id) DO UPDATE SET value=excluded.value")
                    .bind(tenant).bind(workspace).bind(id).bind(r["value"].as_i64().unwrap_or(0))
                    .execute(&mut conn).await?;
                if op == "hold-write" {
                    hold_until_killed()?;
                }
                emit(json!({"ok": true}))?;
            }
            "get" | "search" => {
                let values: Vec<i64> = sqlx::query_scalar("SELECT value FROM records WHERE tenant=? AND workspace=? AND id=? ORDER BY value")
                    .bind(tenant).bind(workspace).bind(id).fetch_all(&mut conn).await?;
                emit(json!({"values": values}))?;
            }
            "delete" => {
                sqlx::query("DELETE FROM records WHERE tenant=? AND workspace=? AND id=?")
                    .bind(tenant)
                    .bind(workspace)
                    .bind(id)
                    .execute(&mut conn)
                    .await?;
                emit(json!({"ok": true}))?;
            }
            _ => bail!("unsupported sqlite operation {op}"),
        }
    }
    conn.close().await?;
    Ok(())
}
