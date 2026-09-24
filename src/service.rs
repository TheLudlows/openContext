use crate::{
    db::{self, Tx},
    error::{AppError, Result},
    models::Models,
    parsing,
    types::*,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;

#[derive(Clone)]
pub struct Service {
    pub pool: PgPool,
    pub files: Arc<PathBuf>,
    pub models: Models,
}
impl Service {
    pub fn new(pool: PgPool, files: PathBuf, models: Models) -> Self {
        Self {
            pool,
            files: Arc::new(files),
            models,
        }
    }
    pub async fn auth(&self, token: &str) -> Result<AuthContext> {
        db::authenticate(&self.pool, token).await
    }
    pub fn file_path(&self, a: &AuthContext, id: Uuid) -> PathBuf {
        self.files
            .join(a.tenant_id.to_string())
            .join(a.workspace_id.to_string())
            .join(id.to_string())
    }

    async fn begin_command(
        &self,
        a: &AuthContext,
        permission: &str,
        op: &str,
        key: &str,
        input: &Value,
    ) -> Result<(Tx, Option<Value>, String)> {
        if key.is_empty() || key.len() > 200 || !key.is_ascii() {
            return Err(AppError::Invalid(
                "Idempotency-Key must contain 1..200 ASCII characters".into(),
            ));
        }
        let mut tx = db::authorized(&self.pool, a, permission, true).await?;
        let hash = db::hash(serde_json::to_vec(input).map_err(anyhow::Error::from)?);
        let old:Option<(String,Value)>=sqlx::query_as("SELECT request_hash,response FROM oc.commands WHERE principal_id=$1 AND operation=$2 AND key=$3")
            .bind(a.id).bind(op).bind(key).fetch_optional(&mut *tx).await?;
        let cached = match old {
            Some((h, response)) if h == hash => Some(response),
            Some(_) => {
                return Err(AppError::Conflict(
                    "idempotency key was used with different input".into(),
                ));
            }
            None => None,
        };
        Ok((tx, cached, hash))
    }
    async fn finish_command(
        tx: &mut Tx,
        a: &AuthContext,
        op: &str,
        key: &str,
        hash: &str,
        response: &Value,
    ) -> Result<()> {
        sqlx::query("INSERT INTO oc.commands(tenant_id,workspace_id,principal_id,operation,key,request_hash,response) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(a.id).bind(op).bind(key).bind(hash).bind(response).execute(&mut **tx).await?;
        Ok(())
    }
    pub(crate) async fn event(
        tx: &mut Tx,
        a: &AuthContext,
        kind: &str,
        content: &str,
        file: Option<Uuid>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oc.events(tenant_id,workspace_id,id,content,kind,created_by,file_id) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(content).bind(kind).bind(a.id).bind(file).execute(&mut **tx).await?;
        Ok(id)
    }
    pub(crate) async fn enqueue(
        tx: &mut Tx,
        a: &AuthContext,
        operation: &str,
        mut payload: Value,
        asset: Option<Uuid>,
        source: Option<Uuid>,
    ) -> Result<Uuid> {
        payload["schema_version"] = json!(1);
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oc.jobs(tenant_id,workspace_id,id,created_by,operation,payload,asset_id,source_event_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(a.id).bind(operation).bind(payload).bind(asset).bind(source).execute(&mut **tx).await?;
        Self::dispatch(
            tx,
            WorkItem {
                tenant_id: a.tenant_id,
                workspace_id: a.workspace_id,
                job_id: id,
                generation: 1,
            },
        )
        .await?;
        Ok(id)
    }
    pub(crate) async fn dispatch(tx: &mut Tx, item: WorkItem) -> Result<()> {
        sqlx::query("SELECT apalis.push_job($1,$2::json,'Pending',now(),5)")
            .bind(db::QUEUE)
            .bind(json!(item))
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    pub(crate) async fn slot(
        tx: &mut Tx,
        a: &AuthContext,
        key: &str,
    ) -> Result<(Uuid, Option<i32>)> {
        let existing: Option<(Uuid, Option<i32>, bool)> = sqlx::query_as(
            "SELECT id,current_version,deleted FROM oc.assets WHERE kind='memory' AND fact_key=$1",
        )
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((id, version, deleted)) = existing {
            if deleted {
                return Err(AppError::Conflict(
                    "fact slot was deleted; use a new fact key".into(),
                ));
            }
            return Ok((id, version));
        }
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oc.assets(tenant_id,workspace_id,id,kind,title,fact_key) VALUES($1,$2,$3,'memory',$4,$4)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(key).execute(&mut **tx).await?;
        Ok((id, None))
    }
    pub(crate) async fn candidate(
        tx: &mut Tx,
        a: &AuthContext,
        input: &MemoryInput,
        source: Uuid,
        approved: bool,
    ) -> Result<(Uuid, Uuid, Option<i32>)> {
        let (asset, version) = Self::slot(tx, a, &input.fact_key).await?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oc.candidates(tenant_id,workspace_id,id,asset_id,source_event_id,fact_key,content,expected_version,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(asset).bind(source).bind(&input.fact_key).bind(&input.content).bind(version).bind(if approved {"approved"} else {"candidate"}).execute(&mut **tx).await?;
        Ok((id, asset, version))
    }
    pub async fn memory(&self, a: &AuthContext, key: &str, input: MemoryInput) -> Result<Value> {
        parsing::validate_text(&input.content)?;
        if input.fact_key.trim().is_empty()
            || input.fact_key.len() > 256
            || input.fact_key.contains('\0')
        {
            return Err(AppError::Invalid(
                "fact_key must contain 1..256 bytes without NUL".into(),
            ));
        }
        let (mut tx, cached, hash) = self
            .begin_command(a, "write", "memory", key, &json!(input))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let source = Self::event(&mut tx, a, "structured", &input.content, None).await?;
        let (asset, version) = Self::slot(&mut tx, a, &input.fact_key).await?;
        // All changes to an existing fact require explicit review, including important decisions.
        let publish = input.publish_if_authorized
            && version.is_none()
            && match db::require_current(&mut tx, a, "publish").await {
                Ok(()) => true,
                Err(AppError::Forbidden) => false,
                Err(error) => return Err(error),
            };
        let (candidate, _, _) = Self::candidate(&mut tx, a, &input, source, publish).await?;
        let job = if publish {
            Some(Self::enqueue(&mut tx,a,"publish",json!({"candidate_id":candidate,"expected_version":version,"format":"text","embedding_profile":self.models.profile}),Some(asset),Some(source)).await?)
        } else {
            None
        };
        let response = json!({"asset_id":asset,"candidate_id":candidate,"source_event_id":source,"state":if publish {"approved"} else {"candidate"},"job_id":job,"conflict":version.is_some()});
        db::audit(
            &mut tx,
            a,
            if publish {
                "memory.direct_authorized"
            } else {
                "memory.candidate"
            },
            candidate,
            json!({"content_hash":db::hash(&input.content),"expected_version":version}),
        )
        .await?;
        Self::finish_command(&mut tx, a, "memory", key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn capture(&self, a: &AuthContext, key: &str, input: CaptureInput) -> Result<Value> {
        parsing::validate_text(&input.content)?;
        let (mut tx, cached, hash) = self
            .begin_command(a, "write", "capture", key, &json!(input))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let source = Self::event(&mut tx, a, "capture", &input.content, None).await?;
        let job = Self::enqueue(
            &mut tx,
            a,
            "extract",
            json!({"schema_version":1}),
            None,
            Some(source),
        )
        .await?;
        let response = json!({"source_event_id":source,"job_id":job});
        db::audit(&mut tx, a, "memory.capture", source, json!({"job_id":job})).await?;
        Self::finish_command(&mut tx, a, "capture", key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn review(
        &self,
        a: &AuthContext,
        key: &str,
        id: Uuid,
        input: ReviewInput,
    ) -> Result<Value> {
        if !["approve", "reject"].contains(&input.decision.as_str())
            || input.reason.trim().is_empty()
            || input.reason.len() > 2000
        {
            return Err(AppError::Invalid(
                "decision must be approve/reject and reason is required (max 2000 bytes)".into(),
            ));
        }
        let op = format!("review/{id}");
        let (mut tx, cached, hash) = self
            .begin_command(a, "review", &op, key, &json!(input))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let row=sqlx::query("SELECT c.*,a.current_version,a.deleted,e.state source_state FROM oc.candidates c JOIN oc.assets a ON a.id=c.asset_id AND a.tenant_id=c.tenant_id AND a.workspace_id=c.workspace_id JOIN oc.events e ON e.id=c.source_event_id AND e.tenant_id=c.tenant_id AND e.workspace_id=c.workspace_id WHERE c.id=$1").bind(id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        if row.get::<String, _>("state") != "candidate"
            || row.get::<i32, _>("revision") != input.expected_revision
            || row.get::<bool, _>("deleted")
            || row.get::<String, _>("source_state") != "active"
        {
            return Err(AppError::Conflict(
                "candidate revision or source is no longer valid".into(),
            ));
        }
        let current: Option<i32> = row.get("current_version");
        if input.expected_version != current {
            return Err(AppError::Conflict(
                "current asset version changed; reread before review".into(),
            ));
        }
        let review = Uuid::new_v4();
        let asset: Uuid = row.get("asset_id");
        let source: Uuid = row.get("source_event_id");
        sqlx::query("INSERT INTO oc.reviews(tenant_id,workspace_id,id,candidate_id,revision,decision,expected_version,reviewer,reason) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(review).bind(id).bind(input.expected_revision).bind(&input.decision).bind(current).bind(a.id).bind(&input.reason).execute(&mut *tx).await?;
        sqlx::query("UPDATE oc.candidates SET state=$2,expected_version=$3 WHERE id=$1")
            .bind(id)
            .bind(if input.decision == "approve" {
                "approved"
            } else {
                "rejected"
            })
            .bind(current)
            .execute(&mut *tx)
            .await?;
        let job = if input.decision == "approve" {
            Some(Self::enqueue(&mut tx,a,"publish",json!({"candidate_id":id,"review_id":review,"expected_version":current,"format":"text","embedding_profile":self.models.profile}),Some(asset),Some(source)).await?)
        } else {
            None
        };
        let response = json!({"review_id":review,"candidate_id":id,"job_id":job,"state":if job.is_some() {"approved"} else {"rejected"}});
        db::audit(&mut tx,a,"memory.review",id,json!({"review_id":review,"decision":input.decision,"content_hash":db::hash(row.get::<String,_>("content")),"expected_version":current})).await?;
        Self::finish_command(&mut tx, a, &op, key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn knowledge(
        &self,
        a: &AuthContext,
        key: &str,
        input: KnowledgeInput,
    ) -> Result<Value> {
        if input.title.trim().is_empty()
            || input.title.len() > 512
            || input.title.contains('\0')
            || !["text", "markdown", "pdf"].contains(&input.format.as_str())
            || input.content.is_some() == input.file_id.is_some()
            || (input.format == "pdf" && input.file_id.is_none())
        {
            return Err(AppError::Invalid("provide title, supported format, and exactly one of content/file_id; PDF requires a file".into()));
        }
        if let Some(text) = &input.content {
            parsing::validate_text(text)?;
        }
        let (mut tx, cached, hash) = self
            .begin_command(a, "write", "knowledge", key, &json!(input))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        if let Some(id) = input.file_id {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oc.files WHERE id=$1 AND NOT deleted)",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if !exists {
                return Err(AppError::NotFound);
            }
        }
        let asset = if let Some(id) = input.asset_id {
            let existing: Option<(Option<i32>, bool, String)> =
                sqlx::query_as("SELECT current_version,deleted,kind FROM oc.assets WHERE id=$1")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let (current, deleted, kind) = existing.ok_or(AppError::NotFound)?;
            if deleted || kind != "knowledge" || current != input.expected_version {
                return Err(AppError::Conflict(
                    "asset deleted, wrong kind or expected_version mismatch".into(),
                ));
            }
            id
        } else {
            if input.expected_version.is_some() {
                return Err(AppError::Invalid(
                    "new asset has no expected_version".into(),
                ));
            }
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO oc.assets(tenant_id,workspace_id,id,kind,title) VALUES($1,$2,$3,'knowledge',$4)").bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(&input.title).execute(&mut *tx).await?;
            id
        };
        let source = Self::event(
            &mut tx,
            a,
            "knowledge",
            input.content.as_deref().unwrap_or(""),
            input.file_id,
        )
        .await?;
        let job=Self::enqueue(&mut tx,a,"ingest",json!({"format":input.format,"title":input.title,"expected_version":input.expected_version,"embedding_profile":self.models.profile}),Some(asset),Some(source)).await?;
        let response = json!({"asset_id":asset,"source_event_id":source,"job_id":job});
        db::audit(&mut tx, a, "knowledge.ingest", asset, json!({"job_id":job})).await?;
        Self::finish_command(&mut tx, a, "knowledge", key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn restore(
        &self,
        a: &AuthContext,
        key: &str,
        id: Uuid,
        input: RestoreInput,
    ) -> Result<Value> {
        if input.reason.trim().is_empty() || input.reason.len() > 2000 {
            return Err(AppError::Invalid(
                "restore reason required (max 2000 bytes)".into(),
            ));
        }
        let op = format!("restore/{id}");
        let (mut tx, cached, hash) = self
            .begin_command(a, "review", &op, key, &json!(input))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let row=sqlx::query("SELECT a.current_version,v.source_event_id,v.title FROM oc.assets a JOIN oc.versions v ON v.asset_id=a.id AND v.tenant_id=a.tenant_id AND v.workspace_id=a.workspace_id JOIN oc.events e ON e.id=v.source_event_id AND e.tenant_id=v.tenant_id AND e.workspace_id=v.workspace_id WHERE a.id=$1 AND v.version=$2 AND NOT a.deleted AND e.state='active'")
            .bind(id).bind(input.target_version).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        if row.get::<Option<i32>, _>("current_version") != Some(input.expected_version) {
            return Err(AppError::Conflict("expected_version mismatch".into()));
        }
        let source: Uuid = row.get("source_event_id");
        let job=Self::enqueue(&mut tx,a,"restore",json!({"target_version":input.target_version,"expected_version":input.expected_version,"title":row.get::<String,_>("title"),"embedding_profile":self.models.profile}),Some(id),Some(source)).await?;
        let response = json!({"asset_id":id,"job_id":job,"restored_from":input.target_version});
        db::audit(&mut tx,a,"asset.restore_authorized",id,json!({"reason":input.reason,"target_version":input.target_version,"expected_version":input.expected_version})).await?;
        Self::finish_command(&mut tx, a, &op, key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn candidate_get(&self, a: &AuthContext, id: Uuid) -> Result<Value> {
        let mut tx = db::authorized(&self.pool, a, "review", false).await?;
        let result:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(c) || jsonb_build_object('current_version',x.current_version) FROM oc.candidates c JOIN oc.assets x ON x.id=c.asset_id AND x.tenant_id=c.tenant_id AND x.workspace_id=c.workspace_id JOIN oc.events e ON e.id=c.source_event_id AND e.tenant_id=c.tenant_id AND e.workspace_id=c.workspace_id WHERE c.id=$1 AND NOT x.deleted AND e.state='active'").bind(id).fetch_optional(&mut *tx).await?;
        result.ok_or(AppError::NotFound)
    }
    pub async fn candidates(&self, a: &AuthContext) -> Result<Value> {
        let mut tx = db::authorized(&self.pool, a, "review", false).await?;
        let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(c) || jsonb_build_object('current_version',x.current_version) FROM oc.candidates c JOIN oc.assets x ON x.id=c.asset_id AND x.tenant_id=c.tenant_id AND x.workspace_id=c.workspace_id JOIN oc.events e ON e.id=c.source_event_id AND e.tenant_id=c.tenant_id AND e.workspace_id=c.workspace_id WHERE c.state='candidate' AND NOT x.deleted AND e.state='active' ORDER BY c.created_at,c.id LIMIT 100").fetch_all(&mut *tx).await?;
        Ok(json!({"items":rows,"limit":100}))
    }
    pub async fn get(&self, a: &AuthContext, id: Uuid, version: Option<i32>) -> Result<Value> {
        let mut tx = db::authorized(&self.pool, a, "read", false).await?;
        let row:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('asset_id',a.id,'kind',a.kind,'title',v.title,'version',v.version,'content',v.content,'source_event_id',v.source_event_id,'restored_from',v.restored_from,'content_hash',v.content_hash) FROM oc.assets a JOIN oc.versions v ON v.asset_id=a.id AND v.tenant_id=a.tenant_id AND v.workspace_id=a.workspace_id AND v.version=COALESCE($2,a.current_version) JOIN oc.events e ON e.id=v.source_event_id AND e.tenant_id=v.tenant_id AND e.workspace_id=v.workspace_id WHERE a.id=$1 AND NOT a.deleted AND e.state='active'").bind(id).bind(version).fetch_optional(&mut *tx).await?;
        row.ok_or(AppError::NotFound)
    }
    pub async fn job(&self, a: &AuthContext, id: Uuid) -> Result<Value> {
        // Jobs can reference unreviewed inputs: readers cannot enumerate them.
        let mut tx = db::authorized(&self.pool, a, "write", false).await?;
        let row:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'operation',operation,'state',state,'generation',generation,'outcome',outcome,'result',result,'error_code',error_code,'cancel_requested',cancel_requested,'updated_at',updated_at) FROM oc.jobs WHERE id=$1").bind(id).fetch_optional(&mut *tx).await?;
        row.ok_or(AppError::NotFound)
    }
    pub async fn job_action(
        &self,
        a: &AuthContext,
        key: &str,
        id: Uuid,
        action: &str,
    ) -> Result<Value> {
        if !["cancel", "retry"].contains(&action) {
            return Err(AppError::Invalid("invalid job action".into()));
        }
        let op = format!("job/{id}/{action}");
        let (mut tx, cached, hash) = self.begin_command(a, "write", &op, key, &json!({})).await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let row = sqlx::query("SELECT * FROM oc.jobs WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if row.get::<Uuid, _>("created_by") != a.id {
            db::require_current(&mut tx, a, "review").await?;
        }
        let state: String = row.get("state");
        if ["completed", "superseded"].contains(&state.as_str())
            || (action == "retry" && !["failed", "cancelled"].contains(&state.as_str()))
        {
            return Err(AppError::Conflict(
                "job state does not allow this action".into(),
            ));
        }
        if row.get::<String, _>("operation") == "cleanup" {
            if action == "cancel" {
                return Err(AppError::Conflict(
                    "deletion cleanup cannot be cancelled".into(),
                ));
            }
            db::require_current(&mut tx, a, "delete").await?;
        }
        if action == "retry" {
            let asset: Option<Uuid> = row.get("asset_id");
            let source: Option<Uuid> = row.get("source_event_id");
            Self::valid_targets(&mut tx, asset, source).await?;
        }
        let generation:i64=sqlx::query_scalar("UPDATE oc.jobs SET state=$2,generation=generation+1,run_token=run_token+1,cancel_requested=$3,error_code=NULL,updated_at=now() WHERE id=$1 RETURNING generation")
            .bind(id).bind(if action=="cancel" {"cancelled"} else {"pending"}).bind(action=="cancel").fetch_one(&mut *tx).await?;
        if action == "retry" {
            Self::dispatch(
                &mut tx,
                WorkItem {
                    tenant_id: a.tenant_id,
                    workspace_id: a.workspace_id,
                    job_id: id,
                    generation,
                },
            )
            .await?;
        }
        let response = json!({"job_id":id,"state":if action=="cancel" {"cancelled"} else {"pending"},"generation":generation});
        db::audit(&mut tx, a, &op, id, json!({"generation":generation})).await?;
        Self::finish_command(&mut tx, a, &op, key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub(crate) async fn valid_targets(
        tx: &mut Tx,
        asset: Option<Uuid>,
        source: Option<Uuid>,
    ) -> Result<()> {
        if let Some(id) = asset {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oc.assets WHERE id=$1 AND NOT deleted)",
            )
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            if !valid {
                return Err(AppError::Conflict("asset is deleted or missing".into()));
            }
        }
        if let Some(id) = source {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM oc.events WHERE id=$1 AND state='active')",
            )
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            if !valid {
                return Err(AppError::Conflict("source is retracted or missing".into()));
            }
        }
        Ok(())
    }
    pub async fn delete(
        &self,
        a: &AuthContext,
        key: &str,
        id: Uuid,
        target: &str,
    ) -> Result<Value> {
        if !["asset", "event", "file"].contains(&target) {
            return Err(AppError::Invalid("invalid deletion target".into()));
        }
        let op = format!("delete/{target}/{id}");
        let (mut tx, cached, hash) = self
            .begin_command(a, "delete", &op, key, &json!({}))
            .await?;
        if let Some(v) = cached {
            return Ok(v);
        }
        let changed = match target {
            "asset" => {
                sqlx::query("UPDATE oc.assets SET deleted=true,current_version=NULL WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
            }
            "event" => sqlx::query("UPDATE oc.events SET state='retracted' WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected(),
            _ => {
                let count = sqlx::query("UPDATE oc.files SET deleted=true WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                sqlx::query("UPDATE oc.events SET state='retracted' WHERE file_id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                count
            }
        };
        if changed == 0 {
            return Err(AppError::NotFound);
        }
        sqlx::query("UPDATE oc.candidates c SET state='withdrawn',revision=revision+1 WHERE state IN ('candidate','approved') AND (EXISTS(SELECT 1 FROM oc.assets a WHERE a.id=c.asset_id AND a.deleted) OR EXISTS(SELECT 1 FROM oc.events e WHERE e.id=c.source_event_id AND e.state='retracted'))").execute(&mut *tx).await?;
        sqlx::query("UPDATE oc.jobs j SET state='cancelled',cancel_requested=true,run_token=run_token+1,updated_at=now() WHERE operation<>'cleanup' AND state IN ('pending','processing','failed','retry_wait') AND (EXISTS(SELECT 1 FROM oc.assets a WHERE a.id=j.asset_id AND a.deleted) OR EXISTS(SELECT 1 FROM oc.events e WHERE e.id=j.source_event_id AND e.state='retracted'))").execute(&mut *tx).await?;
        let job = Self::enqueue(
            &mut tx,
            a,
            "cleanup",
            json!({"target":target,"id":id}),
            None,
            None,
        )
        .await?;
        let response =
            json!({"id":id,"blocked":true,"cleanup_job_id":job,"originals_retained":true});
        db::audit(
            &mut tx,
            a,
            "source.logical_delete",
            id,
            json!({"target":target,"job_id":job}),
        )
        .await?;
        Self::finish_command(&mut tx, a, &op, key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn upload(
        &self,
        a: &AuthContext,
        key: &str,
        name: &str,
        format: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        if name.len() > 512
            || name.contains('\0')
            || bytes.is_empty()
            || bytes.len() > parsing::MAX_FILE
            || !["text", "markdown", "pdf"].contains(&format)
        {
            return Err(AppError::Invalid(
                "invalid file name, format or size (max 10 MB)".into(),
            ));
        }
        if format != "pdf" {
            parsing::validate_text(
                std::str::from_utf8(bytes)
                    .map_err(|_| AppError::Invalid("UTF-8 required".into()))?,
            )?;
        }
        let content_hash = db::hash(bytes);
        let input = json!({"name":name,"format":format,"hash":content_hash});
        // Check permissions before storing untrusted bytes; file IO precedes the atomic metadata transaction.
        db::authorized(&self.pool, a, "write", false)
            .await?
            .rollback()
            .await?;
        let id = Uuid::new_v4();
        let path = self.file_path(a, id);
        tokio::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| AppError::Internal(anyhow::anyhow!("invalid storage path")))?,
        )
        .await
        .map_err(anyhow::Error::from)?;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(anyhow::Error::from)?;
        let (mut tx, cached, hash) = self
            .begin_command(a, "write", "upload", key, &input)
            .await?;
        if let Some(v) = cached {
            let _ = tokio::fs::remove_file(&path).await;
            return Ok(v);
        }
        sqlx::query("INSERT INTO oc.files(tenant_id,workspace_id,id,name,media_type,hash,size,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(a.tenant_id).bind(a.workspace_id).bind(id).bind(name).bind(format).bind(content_hash).bind(bytes.len() as i64).bind(a.id).execute(&mut *tx).await?;
        let response = json!({"file_id":id,"size":bytes.len(),"format":format});
        db::audit(&mut tx, a, "file.upload", id, json!({"size":bytes.len()})).await?;
        Self::finish_command(&mut tx, a, "upload", key, &hash, &response).await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn file(&self, a: &AuthContext, id: Uuid) -> Result<Vec<u8>> {
        // Raw uploads may contain unreviewed material, so only writers/reviewers/admins can download.
        let mut tx = db::authorized(&self.pool, a, "write", false).await?;
        let hash: Option<String> =
            sqlx::query_scalar("SELECT hash FROM oc.files WHERE id=$1 AND NOT deleted")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let expected = hash.ok_or(AppError::NotFound)?;
        tx.commit().await?;
        let bytes = tokio::fs::read(self.file_path(a, id))
            .await
            .map_err(anyhow::Error::from)?;
        if db::hash(&bytes) != expected {
            return Err(AppError::Unavailable(
                "stored file integrity check failed".into(),
            ));
        }
        let mut tx = db::authorized(&self.pool, a, "write", false).await?;
        let visible: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oc.files WHERE id=$1 AND NOT deleted)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if !visible {
            return Err(AppError::NotFound);
        }
        Ok(bytes)
    }
}
