//! Real CLI processes and a localhost model stub: no paid provider calls.
use axum::{Json, Router, extract::State, routing::post};
use opencontext::{db, models::Models, service::Service, types::*};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};
use uuid::Uuid;

fn id(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(value[field].as_str().unwrap()).unwrap()
}
fn command(url: &str, files: &std::path::Path, model: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_opencontext"));
    c.env("DATABASE_URL", url)
        .env("OC_FILES_DIR", files)
        .env("OC_ENABLE_MODELS", "true")
        .env("OC_MODEL_BASE_URL", model)
        .env("OC_EMBEDDING_MODEL", "stub")
        .env("OC_EMBEDDING_DIMENSION", "3")
        .env("OC_EXTRACTION_MODEL", "stub")
        .env("OC_MODEL_API_KEY", "local-test")
        .env("RUST_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    c
}
async fn job(s: &Service, a: &AuthContext, id: Uuid, state: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let value = s.job(a, id).await.unwrap();
            if value["state"] == state {
                return value;
            }
            assert!(
                !["failed", "superseded"].contains(&value["state"].as_str().unwrap()),
                "unexpected job: {value}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("job deadline")
}
async fn stop(child: &mut Child) {
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
async fn embed(State(slow): State<Arc<AtomicBool>>, Json(_): Json<Value>) -> Json<Value> {
    if slow.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    Json(json!({"data":[{"embedding":[1.0,0.5,0.25]}]}))
}
async fn extract() -> Json<Value> {
    // One stub serves extract (memories), extract_graph (entities/relations) and summarize.
    // summarize treats `content` as a plain string; the JSON payload degrades to a
    // low-quality summary, which is acceptable for the crash-recovery flow under test.
    Json(json!({"choices":[{"message":{"content":json!({
            "memories":[{"fact_key":"model.claim","content":"Model proposal requires review","publish_if_authorized":true}],
            "entities":[{"name":"Atlas","entity_type":"service","description":"payments service"}],
            "relations":[{"source":"Atlas","predicate":"owned_by","target":"Atlas"}]
        }).to_string()}}]}))
}
fn pdf() -> Vec<u8> {
    let stream = "BT /F1 12 Tf 72 720 Td (Release approval evidence) Tj ET";
    let objects=["<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream",stream.len())];
    let mut doc = "%PDF-1.4\n".to_string();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(doc.len());
        doc += &format!("{} 0 obj\n{object}\nendobj\n", i + 1);
    }
    let xref = doc.len();
    doc += "xref\n0 6\n0000000000 65535 f \n";
    for offset in offsets {
        doc += &format!("{offset:010} 00000 n \n");
    }
    doc += &format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n");
    doc.into_bytes()
}

#[tokio::test]
#[ignore = "requires isolated TEST_ADMIN_DATABASE_URL and TEST_DATABASE_URL"]
async fn api_worker_models_pdf_mcp_and_crash_recovery() {
    let admin_url = std::env::var("TEST_ADMIN_DATABASE_URL").unwrap();
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let admin = db::connect(&admin_url).await.unwrap();
    let pool = db::connect(&url).await.unwrap();
    let files = tempfile::tempdir().unwrap();
    let s = Service::new(pool.clone(), files.path().into(), Models::disabled());
    let provision = db::provision(&admin, "process tests").await.unwrap();
    let token = provision["token"].as_str().unwrap();
    let a = s.auth(token).await.unwrap();
    let slow = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/embeddings", post(embed))
                .route("/chat/completions", post(extract))
                .with_state(slow.clone()),
        )
        .into_future(),
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    drop(port);
    let base = format!("http://{address}");
    let mut api = command(&url, files.path(), &model)
        .arg("api")
        .arg("--bind")
        .arg(address.to_string())
        .spawn()
        .unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if http
                .get(format!("{base}/health/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let mut worker = command(&url, files.path(), &model)
        .arg("worker")
        .spawn()
        .unwrap();
    async fn send(
        http: &reqwest::Client,
        base: &str,
        token: &str,
        path: &str,
        body: Value,
    ) -> Value {
        let response = http
            .post(format!("{base}{path}"))
            .bearer_auth(token)
            .header("Idempotency-Key", Uuid::new_v4().to_string())
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.json::<Value>().await.unwrap();
        assert!(status.is_success(), "{status}: {body}");
        body
    }
    let published=send(&http,&base,token,"/v1/memories",json!({"fact_key":"policy","content":"Release approval required","publish_if_authorized":true})).await;
    let complete = job(&s, &a, id(&published, "job_id"), "completed").await;
    assert_eq!(
        complete["result"]["index_capabilities"],
        json!(["keyword", "vector"])
    );
    for mode in ["keyword", "vector", "hybrid"] {
        let result = send(
            &http,
            &base,
            token,
            "/v1/search",
            json!({"query":"approval","mode":mode}),
        )
        .await;
        assert_eq!(result["hits"][0]["asset_id"], published["asset_id"]);
        assert_eq!(result["effective_mode"], mode);
    }
    let context = send(
        &http,
        &base,
        token,
        "/v1/resolve",
        json!({"query":"approval","budget_tokens":500}),
    )
    .await;
    assert!(context["count"].as_u64().unwrap() <= 500);
    assert_eq!(context["sources"].as_array().unwrap().len(), 1);

    // The provider's self-asserted publish flag never authorizes extracted candidates.
    let capture = send(
        &http,
        &base,
        token,
        "/v1/captures",
        json!({"content":"A proposed policy"}),
    )
    .await;
    let extracted = job(&s, &a, id(&capture, "job_id"), "completed").await;
    assert_eq!(extracted["outcome"], "candidates_created");
    let candidate_id =
        Uuid::parse_str(extracted["result"]["candidate_ids"][0].as_str().unwrap()).unwrap();
    let candidate = s.candidate_get(&a, candidate_id).await.unwrap();
    assert_eq!(candidate["state"], "candidate");
    assert!(s.get(&a, id(&candidate, "asset_id"), None).await.is_err());

    // Real PDF parsing uses the CLI subprocess and records the page citation.
    let upload = s
        .upload(&a, "pdf-upload", "evidence.pdf", "pdf", &pdf())
        .await
        .unwrap();
    let imported = send(
        &http,
        &base,
        token,
        "/v1/knowledge",
        json!({"title":"PDF evidence","format":"pdf","file_id":upload["file_id"]}),
    )
    .await;
    job(&s, &a, id(&imported, "job_id"), "completed").await;
    assert!(
        s.get(&a, id(&imported, "asset_id"), None).await.unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Release approval evidence")
    );
    let hits = send(
        &http,
        &base,
        token,
        "/v1/search",
        json!({"query":"evidence"}),
    )
    .await;
    assert_eq!(hits["hits"][0]["locator"]["page"], 1);
    let malformed = s
        .upload(&a, "bad-pdf", "bad.pdf", "pdf", b"not a PDF")
        .await
        .unwrap();
    let bad = send(
        &http,
        &base,
        token,
        "/v1/knowledge",
        json!({"title":"Malformed","format":"pdf","file_id":malformed["file_id"]}),
    )
    .await;
    job(&s, &a, id(&bad, "job_id"), "failed").await;

    // A second queue runner is refused before it can requeue a live worker's job.
    let second = command(&url, files.path(), &model)
        .arg("worker")
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(!second.success());

    // Kill during external IO, then let the real queue recover it after restart.
    slow.store(true, Ordering::SeqCst);
    let interrupted = send(
        &http,
        &base,
        token,
        "/v1/knowledge",
        json!({"title":"Recover","content":"recoverable evidence"}),
    )
    .await;
    job(&s, &a, id(&interrupted, "job_id"), "processing").await;
    stop(&mut worker).await;
    slow.store(false, Ordering::SeqCst);
    worker = command(&url, files.path(), &model)
        .arg("worker")
        .spawn()
        .unwrap();
    job(&s, &a, id(&interrupted, "job_id"), "completed").await;
    let runs: i64 = sqlx::query_scalar("SELECT run_token FROM oc.jobs WHERE id=$1")
        .bind(id(&interrupted, "job_id"))
        .fetch_one(&admin)
        .await
        .unwrap();
    assert!(runs >= 2);
    assert_eq!(
        s.get(&a, id(&interrupted, "asset_id"), None).await.unwrap()["version"],
        1
    );

    // Ingest (knowledge) extracts and persists a knowledge graph with provenance owners.
    let entities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oc.entities WHERE workspace_id=$1")
            .bind(a.workspace_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    let relations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oc.relations WHERE workspace_id=$1")
            .bind(a.workspace_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(entities >= 1, "expected ingested knowledge graph entities");
    assert!(
        relations >= 1,
        "expected ingested knowledge graph relations"
    );

    // Delete during model IO; the late handler must not restore visibility.
    slow.store(true, Ordering::SeqCst);
    let deleted = send(
        &http,
        &base,
        token,
        "/v1/knowledge",
        json!({"title":"Deleted","content":"never resurrect"}),
    )
    .await;
    job(&s, &a, id(&deleted, "job_id"), "processing").await;
    s.delete(&a, "delete-running", id(&deleted, "asset_id"), "asset")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(s.get(&a, id(&deleted, "asset_id"), None).await.is_err());
    assert_eq!(
        s.job(&a, id(&deleted, "job_id")).await.unwrap()["state"],
        "cancelled"
    );
    slow.store(false, Ordering::SeqCst);

    // Real MCP JSON-RPC handshake and tools call; revoked tokens fail the next call.
    let mut mcp = command(&url, files.path(), &model)
        .env("OC_API_KEY", token)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.stdout.take().unwrap()).lines();
    async fn rpc(
        input: &mut tokio::process::ChildStdin,
        output: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        value: Value,
    ) -> Value {
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
        let line = tokio::time::timeout(Duration::from_secs(10), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let hello=rpc(&mut input,&mut output,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    assert!(hello.get("result").is_some());
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let tools = rpc(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 3);
    let get = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context_get","arguments":{"asset_id":published["asset_id"]}}});
    let result = rpc(&mut input, &mut output, get.clone()).await;
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Release approval required")
    );
    sqlx::query("UPDATE oc.api_keys SET revoked=true WHERE id=$1")
        .bind(a.id)
        .execute(&admin)
        .await
        .unwrap();
    let denied = rpc(&mut input, &mut output, get).await;
    assert!(denied.get("error").is_some());
    stop(&mut mcp).await;
    stop(&mut worker).await;
    stop(&mut api).await;
    mock.abort();
}
