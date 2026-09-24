use crate::{
    db,
    error::{AppError, Result},
    graph, parsing,
    service::Service,
    types::*,
};
use apalis::prelude::*;
use apalis_sql::{Config, postgres::PostgresStorage};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

struct Claimed {
    auth: AuthContext,
    operation: String,
    payload: Value,
    asset: Option<Uuid>,
    source: Option<Uuid>,
    run_token: i64,
}
enum Prepared {
    // (chunks + optional embeddings, optional graph extraction, per-chunk summaries)
    Publish(
        Vec<(Chunk, Option<pgvector::Vector>)>,
        Option<GraphExtraction>,
        Vec<String>,
    ),
    Candidates(Vec<MemoryInput>),
    Cleanup,
}

pub async fn run(service: Service) -> anyhow::Result<()> {
    // Apalis 0.7.4 requeues all running jobs in its namespace at startup and
    // acknowledges without a lock-owner predicate. Fence concurrent runners
    // until an upstream upgrade validates safe multi-worker startup/ack.
    // Detach so dropping this connection closes it instead of pooling the lock.
    let mut runner_lease = service.pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1,0))")
        .bind(format!("{}:runner", db::QUEUE))
        .fetch_one(&mut runner_lease)
        .await?;
    anyhow::ensure!(
        locked,
        "another worker owns this queue; this version supports one worker process per database"
    );
    let config = Config::new(db::QUEUE)
        .set_buffer_size(1)
        .set_poll_interval(Duration::from_millis(300))
        .set_keep_alive(Duration::from_secs(5))
        .set_reenqueue_orphaned_after(Duration::from_secs(30));
    let storage: PostgresStorage<WorkItem> =
        PostgresStorage::new_with_config(service.pool.clone(), config);
    let monitor = Monitor::new()
        .register(
            WorkerBuilder::new(format!("opencontext-{}", Uuid::new_v4()))
                .data(service)
                .backend(storage)
                .build_fn(handle),
        )
        .run_with_signal(async { tokio::signal::ctrl_c().await });
    tokio::pin!(monitor);
    // Losing the lease must stop the runner; otherwise another process could
    // start while this one continues external work after a database reconnect.
    let heartbeat = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            sqlx::query("SELECT 1").execute(&mut runner_lease).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), sqlx::Error>(())
    };
    tokio::select! {
        result = &mut monitor => result?,
        result = heartbeat => { result?; }
    }
    Ok(())
}
async fn handle(item: WorkItem, service: Data<Service>) -> std::result::Result<(), std::io::Error> {
    process(&service, item)
        .await
        .map_err(|_| std::io::Error::other("job database operation failed"))
}

pub async fn process(service: &Service, item: WorkItem) -> Result<()> {
    let Some(claim) = claim(service, &item).await? else {
        return Ok(());
    };
    let result = prepare(service, &claim).await;
    match result {
        Ok(prepared) => match commit(service, &item, &claim, prepared).await {
            Ok(()) => Ok(()),
            Err(e) => fail(service, &item, &claim, e).await,
        },
        Err(e) => fail(service, &item, &claim, e).await,
    }
}

async fn claim(service: &Service, item: &WorkItem) -> Result<Option<Claimed>> {
    let mut tx = db::scoped(&service.pool, item.tenant_id, item.workspace_id, true).await?;
    let row=sqlx::query("UPDATE oc.jobs SET state='processing',run_token=run_token+1,updated_at=now() WHERE id=$1 AND generation=$2 AND state IN ('pending','processing','retry_wait') AND NOT cancel_requested RETURNING *")
        .bind(item.job_id).bind(item.generation).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let actor: Uuid = row.get("created_by");
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM oc.api_keys WHERE id=$1 AND NOT revoked")
            .bind(actor)
            .fetch_optional(&mut *tx)
            .await?;
    let claim = Claimed {
        auth: AuthContext {
            id: actor,
            tenant_id: item.tenant_id,
            workspace_id: item.workspace_id,
            role: role.unwrap_or_else(|| "revoked".into()),
        },
        operation: row.get("operation"),
        payload: row.get("payload"),
        asset: row.get("asset_id"),
        source: row.get("source_event_id"),
        run_token: row.get("run_token"),
    };
    tx.commit().await?;
    Ok(Some(claim))
}

fn required_permission(claim: &Claimed) -> &'static str {
    match claim.operation.as_str() {
        // Automatic publication is part of an already-authorized write job.
        // Explicit restore remains a privileged operation.
        "restore" => "publish",
        "publish" | "extract" => "write",
        _ => "write",
    }
}
async fn prepare(service: &Service, claim: &Claimed) -> Result<Prepared> {
    if claim.payload["schema_version"] != 1 {
        return Err(AppError::Invalid("unsupported job payload schema".into()));
    }
    if claim.operation == "cleanup" {
        return Ok(Prepared::Cleanup);
    }
    let mut tx = db::authorized(
        &service.pool,
        &claim.auth,
        required_permission(claim),
        false,
    )
    .await?;
    Service::valid_targets(&mut tx, claim.asset, claim.source).await?;
    if claim.operation == "extract" {
        let text: String = sqlx::query_scalar("SELECT content FROM oc.events WHERE id=$1")
            .bind(claim.source)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(Prepared::Candidates(service.models.extract(&text).await?));
    }
    let required: Option<String> =
        serde_json::from_value(claim.payload["embedding_profile"].clone())
            .map_err(anyhow::Error::from)?;
    if required.is_some() && required != service.models.profile {
        return Err(AppError::Unavailable(
            "worker embedding profile differs from accepted job".into(),
        ));
    }
    let chunks = match claim.operation.as_str() {
        "publish" => {
            let text: Option<String> =
                sqlx::query_scalar("SELECT content FROM oc.events WHERE id=$1 AND state='active'")
                    .bind(claim.source)
                    .fetch_optional(&mut *tx)
                    .await?;
            parsing::chunks(
                &text
                    .ok_or_else(|| AppError::Conflict("source event is no longer active".into()))?,
                "text",
            )?
        }
        "restore" => {
            let version = claim.payload["target_version"]
                .as_i64()
                .ok_or_else(|| AppError::Invalid("missing restore version".into()))?
                as i32;
            let rows:Vec<(String,Value)>=sqlx::query_as("SELECT content,locator FROM oc.chunks WHERE asset_id=$1 AND version=$2 ORDER BY ordinal").bind(claim.asset).bind(version).fetch_all(&mut *tx).await?;
            if rows.is_empty() {
                return Err(AppError::Conflict(
                    "historical chunks no longer available".into(),
                ));
            }
            rows.into_iter()
                .map(|(content, locator)| Chunk { content, locator })
                .collect()
        }
        "ingest" => {
            let row=sqlx::query("SELECT e.content,e.file_id,f.hash,f.media_type FROM oc.events e LEFT JOIN oc.files f ON f.id=e.file_id AND f.tenant_id=e.tenant_id AND f.workspace_id=e.workspace_id AND NOT f.deleted WHERE e.id=$1").bind(claim.source).fetch_one(&mut *tx).await?;
            let file: Option<Uuid> = row.get("file_id");
            let format = claim.payload["format"].as_str().unwrap_or("text");
            if let Some(id) = file {
                let expected: Option<String> = row.get("hash");
                let media: Option<String> = row.get("media_type");
                if media.as_deref() != Some(format) {
                    return Err(AppError::Invalid(
                        "file format does not match ingest request".into(),
                    ));
                }
                tx.commit().await?;
                let path = service.file_path(&claim.auth, id);
                let bytes = tokio::fs::read(&path).await.map_err(anyhow::Error::from)?;
                if expected.as_deref() != Some(db::hash(bytes).as_str()) {
                    return Err(AppError::Unavailable(
                        "stored file integrity check failed".into(),
                    ));
                }
                return prepare_ingest(
                    service,
                    parsing::parse_file(&path, format).await?,
                    required.is_some(),
                )
                .await;
            }
            let chunks = parsing::chunks(&row.get::<String, _>("content"), format)?;
            tx.commit().await?;
            return prepare_ingest(service, chunks, required.is_some()).await;
        }
        _ => return Err(AppError::Invalid("unknown operation".into())),
    };
    tx.commit().await?;
    // publish/restore: short facts and historical chunks do not generate summaries or graph.
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let embedding = if required.is_some() {
            Some(service.models.embed(&chunk.content).await?)
        } else {
            None
        };
        out.push((chunk, embedding));
    }
    Ok(Prepared::Publish(out, None, Vec::new()))
}
async fn prepare_ingest(service: &Service, chunks: Vec<Chunk>, required: bool) -> Result<Prepared> {
    let mut out = Vec::with_capacity(chunks.len());
    let mut summaries = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let embedding = if required {
            Some(service.models.embed(&chunk.content).await?)
        } else {
            None
        };
        let summary = if service.models.extraction_enabled() {
            match service.models.summarize(&chunk.content).await {
                Ok(s) => s,
                // Summary is best-effort: a provider failure does not block publishing.
                Err(AppError::Unavailable(_)) => String::new(),
                Err(e) => return Err(e),
            }
        } else {
            String::new()
        };
        summaries.push(summary);
        out.push((chunk.clone(), embedding));
    }
    let full_text = chunks
        .iter()
        .map(|c| c.content.as_str())
        .collect::<Vec<_>>()
        .join("");
    let graph = if service.models.extraction_enabled() {
        match service.models.extract_graph(&full_text).await {
            Ok(g) => Some(g),
            // Graph is best-effort: keyword path still publishes without a graph.
            Err(AppError::Unavailable(_)) => None,
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    Ok(Prepared::Publish(out, graph, summaries))
}

async fn commit(
    service: &Service,
    item: &WorkItem,
    claim: &Claimed,
    prepared: Prepared,
) -> Result<()> {
    let mut tx = if claim.operation == "cleanup" {
        db::scoped(&service.pool, item.tenant_id, item.workspace_id, true).await?
    } else {
        db::authorized(&service.pool, &claim.auth, required_permission(claim), true).await?
    };
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oc.jobs WHERE id=$1 AND generation=$2 AND run_token=$3 AND state='processing' AND NOT cancel_requested)")
        .bind(item.job_id).bind(item.generation).bind(claim.run_token).fetch_one(&mut *tx).await?;
    if !active {
        return Ok(());
    }
    if claim.operation != "cleanup" {
        Service::valid_targets(&mut tx, claim.asset, claim.source).await?;
    }
    let (outcome, result) = match prepared {
        Prepared::Candidates(candidates) => {
            let source = claim.source.ok_or(AppError::NotFound)?;
            let mut ids = Vec::new();
            for candidate in candidates {
                let (id, _, _) =
                    Service::candidate(&mut tx, &claim.auth, &candidate, source, false).await?;
                ids.push(id);
            }
            ("candidates_created", json!({"candidate_ids":ids}))
        }
        Prepared::Publish(chunks, graph, summaries) => {
            let asset = claim.asset.ok_or(AppError::NotFound)?;
            let source = claim.source.ok_or(AppError::NotFound)?;
            let current: Option<i32> =
                sqlx::query_scalar("SELECT current_version FROM oc.assets WHERE id=$1")
                    .bind(asset)
                    .fetch_one(&mut *tx)
                    .await?;
            let expected: Option<i32> =
                serde_json::from_value(claim.payload["expected_version"].clone())
                    .map_err(anyhow::Error::from)?;
            if current != expected {
                return Err(AppError::Conflict(
                    "publication superseded by another version".into(),
                ));
            }
            let version = current.unwrap_or(0) + 1;
            let content = chunks
                .iter()
                .map(|(c, _)| c.content.as_str())
                .collect::<Vec<_>>()
                .join("");
            let restored = claim
                .payload
                .get("target_version")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);
            sqlx::query("INSERT INTO oc.versions(tenant_id,workspace_id,asset_id,version,content,content_hash,source_event_id,restored_from,created_by,title) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,COALESCE($10,(SELECT title FROM oc.assets WHERE id=$3)))")
                .bind(item.tenant_id).bind(item.workspace_id).bind(asset).bind(version).bind(&content).bind(db::hash(&content)).bind(source).bind(restored).bind(claim.auth.id).bind(claim.payload.get("title").and_then(|v|v.as_str())).execute(&mut *tx).await?;
            let summary_model = service.models.summary_model().unwrap_or("");
            for (ordinal, (chunk, embedding)) in chunks.into_iter().enumerate() {
                let chunk_id = Uuid::new_v4();
                let profile = if embedding.is_some() {
                    service.models.profile.as_deref()
                } else {
                    None
                };
                sqlx::query("INSERT INTO oc.chunks(tenant_id,workspace_id,id,asset_id,version,ordinal,content,locator,search_vector,embedding,embedding_profile) VALUES($1,$2,$3,$4,$5,$6,$7,$8,to_tsvector('simple',$9),$10,$11)")
                    .bind(item.tenant_id).bind(item.workspace_id).bind(chunk_id).bind(asset).bind(version).bind(ordinal as i32).bind(&chunk.content).bind(chunk.locator).bind(parsing::lexical(&chunk.content)).bind(embedding).bind(profile).execute(&mut *tx).await?;
                if let Some(summary) = summaries.get(ordinal).filter(|s| !s.is_empty()) {
                    sqlx::query("INSERT INTO oc.summaries(tenant_id,workspace_id,id,chunk_id,text,model_revision,search_vector) VALUES($1,$2,$3,$4,$5,$6,to_tsvector('simple',$7))")
                        .bind(item.tenant_id).bind(item.workspace_id).bind(Uuid::new_v4()).bind(chunk_id).bind(summary).bind(summary_model).bind(parsing::lexical(summary)).execute(&mut *tx).await?;
                }
            }
            // Graph extraction writes provenance owners so source retraction can cascade.
            let graph_result = if let Some(g) = graph {
                Some(graph::write_graph(&mut tx, &claim.auth, source, &g).await?)
            } else {
                None
            };
            sqlx::query(
                "UPDATE oc.assets SET current_version=$2,title=COALESCE($3,title) WHERE id=$1",
            )
            .bind(asset)
            .bind(version)
            .bind(claim.payload.get("title").and_then(|v| v.as_str()))
            .execute(&mut *tx)
            .await?;
            db::audit(&mut tx,&claim.auth,"asset.published",asset,json!({"version":version,"source_event_id":source,"job_id":item.job_id,"restored_from":restored})).await?;
            (
                "published",
                json!({"asset_id":asset,"version":version,"readiness":"ready","index_capabilities":if claim.payload["embedding_profile"].is_string() {vec!["keyword","vector"]} else {vec!["keyword"]},"graph":graph_result}),
            )
        }
        Prepared::Cleanup => {
            // All revoked derived index entries in this scope are safe to remove; originals remain under retention policy.
            let count=sqlx::query("DELETE FROM oc.chunks c WHERE EXISTS(SELECT 1 FROM oc.assets a WHERE a.id=c.asset_id AND a.deleted) OR EXISTS(SELECT 1 FROM oc.versions v JOIN oc.events e ON e.id=v.source_event_id AND e.tenant_id=v.tenant_id AND e.workspace_id=v.workspace_id WHERE v.asset_id=c.asset_id AND v.version=c.version AND e.state='retracted')").execute(&mut *tx).await?.rows_affected();
            // Graph cleanup: retract owners of retracted sources, then delete orphan relations/entities.
            // Shared graph objects with an owner from another active source are retained.
            let graph_cleanup = graph::cleanup_orphans(&mut tx).await?;
            (
                "derived_indexes_removed",
                json!({"chunks_removed":count,"originals_retained":true,"retention_policy":"manual-unconfigured","graph_cleanup":graph_cleanup}),
            )
        }
    };
    sqlx::query("UPDATE oc.jobs SET state='completed',outcome=$2,result=$3,error_code=NULL,updated_at=now() WHERE id=$1").bind(item.job_id).bind(outcome).bind(result).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn fail(service: &Service, item: &WorkItem, claim: &Claimed, error: AppError) -> Result<()> {
    // Database failures are retryable by Apalis. Provider/parser failures require explicit retry, preventing surprise model spend.
    if matches!(error, AppError::Database(_)) {
        return Err(error);
    }
    let mut tx = db::scoped(&service.pool, item.tenant_id, item.workspace_id, true).await?;
    let state = if matches!(error, AppError::Conflict(_)) {
        "superseded"
    } else {
        "failed"
    };
    sqlx::query("UPDATE oc.jobs SET state=$4,error_code=$5,updated_at=now() WHERE id=$1 AND generation=$2 AND run_token=$3 AND state='processing'")
        .bind(item.job_id).bind(item.generation).bind(claim.run_token).bind(state).bind(error.code()).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::warn!(job_id=%item.job_id,code=error.code(),"job did not publish");
    Ok(())
}
