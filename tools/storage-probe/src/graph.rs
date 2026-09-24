use crate::*;
use kuzu::{Connection, Database, SystemConfig, Value as KValue};

pub fn run(path: &Path, request: Value) -> Result<()> {
    let read_only = request["read_only"].as_bool().unwrap_or(false);
    let db = Database::new(
        path,
        SystemConfig::default()
            .buffer_pool_size(64 * 1024 * 1024)
            .max_num_threads(2)
            .read_only(read_only),
    )?;
    let conn = Connection::new(&db)?;
    if request["op"] == "init" {
        conn.query("CREATE NODE TABLE IF NOT EXISTS Record(uid STRING, tenant STRING, workspace STRING, id STRING, value INT64, PRIMARY KEY(uid))")?;
        conn.query("CREATE REL TABLE IF NOT EXISTS Linked(FROM Record TO Record)")?;
        return emit(json!({"ok": true}));
    }
    if request["op"] == "threads" {
        let uid = json!([
            field(&request, "tenant")?,
            field(&request, "workspace")?,
            field(&request, "id")?
        ])
        .to_string();
        let read = || -> Result<Vec<i64>> {
            let reader = Connection::new(&db)?;
            let mut statement = reader.prepare("MATCH (n:Record {uid:$uid}) RETURN n.value")?;
            let rows =
                reader.execute(&mut statement, vec![("uid", KValue::String(uid.clone()))])?;
            Ok(rows
                .filter_map(|row| match row.first() {
                    Some(KValue::Int64(value)) => Some(*value),
                    _ => None,
                })
                .collect())
        };
        conn.query("BEGIN TRANSACTION")?;
        let mut update = conn.prepare("MATCH (n:Record {uid:$uid}) SET n.value=$value")?;
        conn.execute(
            &mut update,
            vec![
                ("uid", KValue::String(uid.clone())),
                (
                    "value",
                    KValue::Int64(request["value"].as_i64().unwrap_or(0)),
                ),
            ],
        )?;
        let before = std::thread::scope(|scope| scope.spawn(read).join())
            .map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
        conn.query("COMMIT")?;
        let after = std::thread::scope(|scope| scope.spawn(read).join())
            .map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
        return emit(json!({"before": before, "after": after}));
    }
    let mut requests = Requests::new(request)?;
    while let Some(r) = requests.next()? {
        let op = field(&r, "op")?;
        let tenant = field(&r, "tenant")?;
        let workspace = field(&r, "workspace")?;
        let id = field(&r, "id")?;
        let uid = json!([tenant, workspace, id]).to_string();
        let mut params = vec![
            ("tenant", KValue::String(tenant.to_owned())),
            ("workspace", KValue::String(workspace.to_owned())),
            ("id", KValue::String(id.to_owned())),
        ];
        let query = match op {
            "put" | "hold-write" => {
                if op == "hold-write" {
                    conn.query("BEGIN TRANSACTION")?;
                }
                params.push(("uid", KValue::String(uid)));
                params.push(("value", KValue::Int64(r["value"].as_i64().unwrap_or(0))));
                "MERGE (n:Record {uid:$uid}) SET n.tenant=$tenant,n.workspace=$workspace,n.id=$id,n.value=$value"
            }
            "get" | "search" => {
                "MATCH (n:Record) WHERE n.tenant=$tenant AND n.workspace=$workspace AND n.id=$id RETURN n.value ORDER BY n.value"
            }
            "delete" => {
                "MATCH (n:Record) WHERE n.tenant=$tenant AND n.workspace=$workspace AND n.id=$id DETACH DELETE n"
            }
            "link" => {
                params.push(("target", KValue::String(field(&r, "target")?.to_owned())));
                "MATCH (a:Record),(b:Record) WHERE a.tenant=$tenant AND a.workspace=$workspace AND a.id=$id AND b.tenant=$tenant AND b.workspace=$workspace AND b.id=$target CREATE (a)-[:Linked]->(b)"
            }
            "neighbors" => {
                "MATCH (a:Record)-[:Linked]->(b:Record) WHERE a.tenant=$tenant AND a.workspace=$workspace AND a.id=$id AND b.tenant=$tenant AND b.workspace=$workspace RETURN b.value ORDER BY b.value"
            }
            _ => bail!("unsupported kuzu operation {op}"),
        };
        let mut prepared = conn.prepare(query)?;
        let result = conn.execute(&mut prepared, params)?;
        let values: Vec<i64> = result
            .filter_map(|row| match row.first() {
                Some(KValue::Int64(n)) => Some(*n),
                _ => None,
            })
            .collect();
        if op == "hold-write" {
            hold_until_killed()?;
        }
        emit(json!({"ok": true, "values": values}))?;
    }
    Ok(())
}
