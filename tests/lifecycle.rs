use axum::{body::Body, http::Request};
use opencontext::{api, db, error::AppError, models::Models, service::Service, types::*, worker};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn id(v: &Value, k: &str) -> Uuid {
    Uuid::parse_str(v[k].as_str().unwrap()).unwrap()
}
fn memory(content: &str, publish: bool) -> MemoryInput {
    MemoryInput {
        fact_key: "release.policy".into(),
        content: content.into(),
        publish_if_authorized: publish,
    }
}
fn search(query: &str) -> SearchInput {
    SearchInput {
        query: query.into(),
        limit: 10,
        mode: "keyword".into(),
        allow_partial: false,
    }
}
async fn execute(s: &Service, a: &AuthContext, j: Uuid, generation: i64) {
    worker::process(
        s,
        WorkItem {
            tenant_id: a.tenant_id,
            workspace_id: a.workspace_id,
            job_id: j,
            generation,
        },
    )
    .await
    .unwrap();
}
async fn key(admin: &PgPool, s: &Service, workspace: Uuid, role: &str) -> (AuthContext, String) {
    let value = db::issue_key(admin, workspace, role).await.unwrap();
    let token = value["token"].as_str().unwrap().to_string();
    (s.auth(&token).await.unwrap(), token)
}

#[tokio::test]
#[ignore = "requires isolated TEST_ADMIN_DATABASE_URL and TEST_DATABASE_URL; never use production"]
async fn postgres_lifecycle_security_and_atomicity() {
    let admin =
        db::connect(&std::env::var("TEST_ADMIN_DATABASE_URL").expect("test administrator URL"))
            .await
            .unwrap();
    db::initialize(&admin).await.unwrap();
    let pool = db::connect(&std::env::var("TEST_DATABASE_URL").expect("non-owner runtime URL"))
        .await
        .unwrap();
    db::check_runtime(&pool).await.unwrap();
    assert!(db::check_runtime(&admin).await.is_err());
    let files = tempfile::tempdir().unwrap();
    let s = Service::new(pool.clone(), files.path().into(), Models::disabled());
    let provision = db::provision(&admin, "lifecycle tests").await.unwrap();
    let a = s.auth(provision["token"].as_str().unwrap()).await.unwrap();
    let (reader, reader_token) = key(&admin, &s, a.workspace_id, "reader").await;
    let (writer, _) = key(&admin, &s, a.workspace_id, "writer").await;
    let other = db::provision(&admin, "other workspace").await.unwrap();
    let b = s.auth(other["token"].as_str().unwrap()).await.unwrap();

    // RLS default deny and pooled transaction-local identity, composite foreign keys.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oc.assets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let mut tx = db::authorized(&pool, &a, "write", true).await.unwrap();
    let unauthorized=sqlx::query("INSERT INTO oc.events(tenant_id,workspace_id,id,content,kind,created_by) VALUES($1,$2,$3,'x','capture',$4)").bind(b.tenant_id).bind(b.workspace_id).bind(Uuid::new_v4()).bind(a.id).execute(&mut *tx).await;
    assert!(unauthorized.is_err());
    tx.rollback().await.unwrap();

    // Writer cannot self-approve; candidates never leak through normal get/search or reader review.
    let candidate = s
        .memory(&writer, "writer-1", memory("发布必须审批", true))
        .await
        .unwrap();
    assert_eq!(candidate["state"], "candidate");
    assert!(candidate["job_id"].is_null());
    assert!(matches!(
        s.candidate_get(&reader, id(&candidate, "candidate_id"))
            .await,
        Err(AppError::Forbidden)
    ));
    assert!(matches!(
        s.get(&reader, id(&candidate, "asset_id"), None).await,
        Err(AppError::NotFound)
    ));
    assert!(
        s.search(&reader, search("审批")).await.unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let review = s
        .review(
            &a,
            "approve-1",
            id(&candidate, "candidate_id"),
            ReviewInput {
                decision: "approve".into(),
                expected_revision: 1,
                expected_version: None,
                reason: "approved by test".into(),
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&review, "job_id"), 1).await;
    assert_eq!(
        s.job(&a, id(&review, "job_id")).await.unwrap()["state"],
        "completed"
    );
    let asset = id(&candidate, "asset_id");
    assert_eq!(s.get(&reader, asset, None).await.unwrap()["version"], 1);
    assert!(
        !s.search(&reader, search("审批")).await.unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        s.get(&b, asset, None).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        s.candidate_get(&b, id(&candidate, "candidate_id")).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        s.job(&b, id(&review, "job_id")).await,
        Err(AppError::NotFound)
    ));
    assert!(
        s.search(&b, search("审批")).await.unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Idempotency survives publication; different content conflicts; concurrent same key gives same result.
    assert_eq!(
        candidate,
        s.memory(&writer, "writer-1", memory("发布必须审批", true))
            .await
            .unwrap()
    );
    assert!(matches!(
        s.memory(&writer, "writer-1", memory("changed", true)).await,
        Err(AppError::Conflict(_))
    ));
    let input = KnowledgeInput {
        title: "Runbook".into(),
        content: Some("Deploy alpha build".into()),
        file_id: None,
        format: "text".into(),
        asset_id: None,
        expected_version: None,
    };
    let (left, right) = tokio::join!(
        s.knowledge(&a, "concurrent", input.clone()),
        s.knowledge(&a, "concurrent", input.clone())
    );
    let first = left.unwrap();
    assert_eq!(first, right.unwrap());
    execute(&s, &a, id(&first, "job_id"), 1).await;
    execute(&s, &a, id(&first, "job_id"), 1).await;
    assert_eq!(
        s.get(&a, id(&first, "asset_id"), None).await.unwrap()["version"],
        1
    );

    // All changed values require review; stale review refuses to overwrite. Restore appends v3.
    let changed = s
        .memory(&a, "changed", memory("发布需要双人审批", true))
        .await
        .unwrap();
    assert_eq!(changed["state"], "candidate");
    assert_eq!(
        s.get(&a, asset, None).await.unwrap()["content"],
        "发布必须审批"
    );
    assert!(
        s.review(
            &a,
            "stale",
            id(&changed, "candidate_id"),
            ReviewInput {
                decision: "approve".into(),
                expected_revision: 1,
                expected_version: None,
                reason: "stale".into()
            }
        )
        .await
        .is_err()
    );
    let approval = s
        .review(
            &a,
            "approved",
            id(&changed, "candidate_id"),
            ReviewInput {
                decision: "approve".into(),
                expected_revision: 1,
                expected_version: Some(1),
                reason: "require two reviewers".into(),
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&approval, "job_id"), 1).await;
    assert_eq!(s.get(&a, asset, None).await.unwrap()["version"], 2);
    let restore = s
        .restore(
            &a,
            "restore",
            asset,
            RestoreInput {
                target_version: 1,
                expected_version: 2,
                reason: "restore first version".into(),
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&restore, "job_id"), 1).await;
    let restored = s.get(&a, asset, None).await.unwrap();
    assert_eq!(restored["version"], 3);
    assert_eq!(restored["restored_from"], 1);
    assert_eq!(
        s.get(&a, asset, Some(2)).await.unwrap()["content"],
        "发布需要双人审批"
    );

    // Source retraction hides current and historical versions immediately, before cleanup.
    let deleted = s
        .delete(&a, "retract", id(&candidate, "source_event_id"), "event")
        .await
        .unwrap();
    assert!(matches!(
        s.get(&a, asset, None).await,
        Err(AppError::NotFound)
    ));
    assert!(s.get(&a, asset, Some(1)).await.is_err());
    assert!(s.get(&a, asset, Some(2)).await.is_ok());
    assert!(
        s.restore(
            &a,
            "restore-retracted",
            asset,
            RestoreInput {
                target_version: 1,
                expected_version: 3,
                reason: "invalid".into()
            }
        )
        .await
        .is_err()
    );
    execute(&s, &a, id(&deleted, "cleanup_job_id"), 1).await;

    // Tombstone invalidates queued publication, retries and every history path.
    let late = s
        .knowledge(
            &a,
            "late",
            KnowledgeInput {
                title: "Late".into(),
                ..input.clone()
            },
        )
        .await
        .unwrap();
    s.delete(&a, "delete-late", id(&late, "asset_id"), "asset")
        .await
        .unwrap();
    execute(&s, &a, id(&late, "job_id"), 1).await;
    assert!(s.get(&a, id(&late, "asset_id"), None).await.is_err());
    assert!(
        s.job_action(&a, "retry-deleted", id(&late, "job_id"), "retry")
            .await
            .is_err()
    );
    s.delete(&a, "delete-history", asset, "asset")
        .await
        .unwrap();
    assert!(s.get(&a, asset, Some(2)).await.is_err());

    // Cancellation fences old queue generations; explicit retry uses a new generation.
    let cancelled = s
        .knowledge(&a, "cancel-knowledge", input.clone())
        .await
        .unwrap();
    let job = id(&cancelled, "job_id");
    s.job_action(&a, "cancel", job, "cancel").await.unwrap();
    execute(&s, &a, job, 1).await;
    assert!(s.get(&a, id(&cancelled, "asset_id"), None).await.is_err());
    let retry = s.job_action(&a, "retry", job, "retry").await.unwrap();
    assert_eq!(retry["generation"], 3);
    execute(&s, &a, job, 1).await;
    assert!(s.get(&a, id(&cancelled, "asset_id"), None).await.is_err());
    execute(&s, &a, job, 3).await;
    assert_eq!(
        s.get(&a, id(&cancelled, "asset_id"), None).await.unwrap()["version"],
        1
    );

    // File IDs cannot escape scope or storage root; deleting a file retracts all its sources.
    let file = s
        .upload(
            &a,
            "upload",
            "../../untrusted.md",
            "markdown",
            b"# Security\nRelease audit record",
        )
        .await
        .unwrap();
    let file_id = id(&file, "file_id");
    assert!(s.file(&b, file_id).await.is_err());
    assert!(s.file(&reader, file_id).await.is_err());
    let imported = s
        .knowledge(
            &a,
            "import",
            KnowledgeInput {
                title: "Security".into(),
                content: None,
                file_id: Some(file_id),
                format: "markdown".into(),
                asset_id: None,
                expected_version: None,
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&imported, "job_id"), 1).await;
    assert!(s.get(&a, id(&imported, "asset_id"), None).await.is_ok());
    s.delete(&a, "delete-file", file_id, "file").await.unwrap();
    assert!(s.file(&a, file_id).await.is_err());
    assert!(s.get(&a, id(&imported, "asset_id"), None).await.is_err());

    // Capture without explicitly configured models fails, with no published fallback.
    let captured = s
        .capture(
            &a,
            "capture",
            CaptureInput {
                content: "Never auto-publish this claim".into(),
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&captured, "job_id"), 1).await;
    assert_eq!(
        s.job(&a, id(&captured, "job_id")).await.unwrap()["state"],
        "failed"
    );
    let semantic = SearchInput {
        mode: "hybrid".into(),
        ..search("alpha")
    };
    assert!(s.search(&a, semantic.clone()).await.is_err());
    assert_eq!(
        s.search(
            &a,
            SearchInput {
                allow_partial: true,
                ..semantic
            }
        )
        .await
        .unwrap()["effective_mode"],
        "keyword"
    );

    // Queue and business mutation roll back together.
    let mut tx = db::authorized(&pool, &a, "write", true).await.unwrap();
    let fake = Uuid::new_v4();
    sqlx::query("INSERT INTO oc.jobs(tenant_id,workspace_id,id,created_by,operation,payload) VALUES($1,$2,$3,$4,'extract','{}')").bind(a.tenant_id).bind(a.workspace_id).bind(fake).bind(a.id).execute(&mut *tx).await.unwrap();
    let queue_id: String = sqlx::query_scalar("SELECT (apalis.push_job($1,$2::json)).id")
        .bind(db::QUEUE)
        .bind(json!(WorkItem {
            tenant_id: a.tenant_id,
            workspace_id: a.workspace_id,
            job_id: fake,
            generation: 1
        }))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(s.job(&a, fake).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM apalis.jobs WHERE id=$1")
        .bind(queue_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Chunking must preserve exact long UTF-8 content, including whitespace-only ranges.
    let original = format!("{}{}{}", "多字节内容".repeat(200), " ".repeat(5000), "tail");
    let long = s
        .knowledge(
            &a,
            "long",
            KnowledgeInput {
                title: "Original title".into(),
                content: Some(original.clone()),
                ..input.clone()
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&long, "job_id"), 1).await;
    let long_id = id(&long, "asset_id");
    assert_eq!(s.get(&a, long_id, None).await.unwrap()["content"], original);
    let updated = s
        .knowledge(
            &a,
            "long-update",
            KnowledgeInput {
                title: "New title".into(),
                asset_id: Some(long_id),
                expected_version: Some(1),
                ..input.clone()
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&updated, "job_id"), 1).await;
    assert_eq!(
        s.get(&a, long_id, Some(1)).await.unwrap()["title"],
        "Original title"
    );
    let undo = s
        .restore(
            &a,
            "long-restore",
            long_id,
            RestoreInput {
                target_version: 1,
                expected_version: 2,
                reason: "restore complete snapshot".into(),
            },
        )
        .await
        .unwrap();
    execute(&s, &a, id(&undo, "job_id"), 1).await;
    let snapshot = s.get(&a, long_id, None).await.unwrap();
    assert_eq!(snapshot["content"], original);
    assert_eq!(snapshot["title"], "Original title");
    assert_eq!(snapshot["version"], 3);

    // An old reviewer AuthContext must not retain privileges after a role downgrade.
    let (demoted, _) = key(&admin, &s, a.workspace_id, "reviewer").await;
    sqlx::query("UPDATE oc.api_keys SET role='writer' WHERE id=$1")
        .bind(demoted.id)
        .execute(&admin)
        .await
        .unwrap();
    let unapproved = s
        .memory(
            &demoted,
            "downgraded-publish",
            MemoryInput {
                fact_key: "new.fact".into(),
                content: "must remain a candidate".into(),
                publish_if_authorized: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(unapproved["state"], "candidate");
    let owned = s.knowledge(&a, "owned-job", input.clone()).await.unwrap();
    assert!(matches!(
        s.job_action(
            &demoted,
            "downgraded-cancel",
            id(&owned, "job_id"),
            "cancel"
        )
        .await,
        Err(AppError::Forbidden)
    ));

    // HTTP auth and reader privileges; token revocation is enforced even for an existing AuthContext.
    let app = api::router(s.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{}", id(&first, "asset_id")))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/candidates")
                .header("Authorization", format!("Bearer {reader_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    sqlx::query("UPDATE oc.api_keys SET revoked=true WHERE id=$1")
        .bind(reader.id)
        .execute(&admin)
        .await
        .unwrap();
    assert!(s.auth(&reader_token).await.is_err());
    assert!(s.get(&reader, id(&first, "asset_id"), None).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oc.assets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
