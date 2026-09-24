# 跨库账本与对账（M3）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 SQLite 关系库落地跨库一致性账本（`artifact_owners` / `index_entries` / `artifact_ledger`），实现 pending→committed 状态机、重启对账收敛与删除依赖顺序清理，供 M4 的 LanceDB/Kuzu 适配器包裹使用。

**Architecture:** 关系库是最终可见性权威。业务事务内登记待写产物（`register_pending`）与溯源（`register_owner`）；Worker 在事务外用确定性 ID 幂等写外部存储；随后关系事务复核来源有效性并确认（`confirm_committed`）或降级（`fail_retryable`/`fail_permanent`）；`reconcile_ledger` 在启动/对账时把卡住的 pending/retry_wait 收敛到终态。本阶段只交付存储层账本能力与单元/集成测试，**不修改 PG Worker**（Worker 编排接入属 M5）。

**Tech Stack:** Rust（edition 2024，rust-version 1.88）、SQLx（SQLite）、`tempfile`（测试）、`uuid`、`serde`。

## Global Constraints

- Rust 最低 1.88；edition 2024；不新增任何依赖（复用现有 SQLx/serde/uuid/sha2）。
- 验证必须全绿：`cargo fmt --all -- --check`、`cargo clippy --locked --offline --all-targets -- -D warnings`、`cargo test --locked --offline`。
- SQLite 编码约定（A5）：UUID→BLOB、JSON→TEXT、布尔→INTEGER(0/1)、时间→INTEGER(unix epoch ms)。所有表带 `tenant_id`/`workspace_id`（scope 隔离，无 RLS）。
- 表名用 `oc_` 前缀；新表加进 `src/storage/sqlite-schema.sql` 并同步 `REQUIRED_TABLES`。
- 领域接口只暴露 `LedgerKey`/`LedgerEntry`/`Surface`/`LedgerState` 等类型，不暴露 SQL/连接/厂商类型（A2.2）。
- 给 `DomainTx` trait 加方法时，必须同步三处：`src/storage/traits.rs`（trait 声明）、`src/storage/sqlite.rs`（`SqliteTx` 实现）、`tests/storage_contract.rs`（`MemTx` double 的 stub，返回 `Err(StorageError::Unavailable(...))`）。
- 每个 Task 独立提交；commit 信息以 `Co-Authored-By: Claude Code <noreply@anthropic.com>` 结尾。

---

## 数据模型（三张新表，Task 1 落地）

```sql
CREATE TABLE oc_artifact_owners (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    chunk_id BLOB,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_type, artifact_id)
);
CREATE INDEX oc_owners_artifact ON oc_artifact_owners(tenant_id, workspace_id, artifact_type, artifact_id);

CREATE TABLE oc_index_entries (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    artifact_id BLOB NOT NULL,
    field TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dimension INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','ready','removed')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, artifact_id, field, model_id, generation)
);

CREATE TABLE oc_artifact_ledger (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    surface TEXT NOT NULL CHECK (surface IN ('vector','graph')),
    generation INTEGER NOT NULL DEFAULT 1,
    idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','committed','retry_wait','orphan')),
    attempt INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_id, surface, generation)
);
CREATE INDEX oc_ledger_state ON oc_artifact_ledger(state, next_retry_at);
```

`SourceVersion { source_id, version }` 中 `source_id` 是来源事件 id（`oc_events.id`，即旧模型的 `source_event_id`），`version` 是资产版本号；图产物（entity/relation）抽取自某个 event，owner/ledger 一并挂该 `(source_id, version)`。

---

### Task 1: 三张账本/溯源表结构

**Files:**
- Modify: `src/storage/sqlite-schema.sql`（追加三张表 + 索引）
- Modify: `src/storage/sqlite.rs:36-65`（`REQUIRED_TABLES` 加三个表名）

**Interfaces:**
- Produces: 表 `oc_artifact_owners`、`oc_index_entries`、`oc_artifact_ledger`（后续 Task 的 SQL 依赖）；`REQUIRED_TABLES` 含三者，故 `check()` 拒绝缺失结构。

- [x] **Step 1: 追加 schema**

在 `src/storage/sqlite-schema.sql` 末尾追加三张表和两个索引：

```sql
CREATE TABLE oc_artifact_owners (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    chunk_id BLOB,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_type, artifact_id)
);
CREATE INDEX oc_owners_artifact ON oc_artifact_owners(tenant_id, workspace_id, artifact_type, artifact_id);

CREATE TABLE oc_index_entries (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    artifact_id BLOB NOT NULL,
    field TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dimension INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','ready','removed')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, artifact_id, field, model_id, generation)
);

CREATE TABLE oc_artifact_ledger (
    tenant_id BLOB NOT NULL, workspace_id BLOB NOT NULL,
    source_id BLOB NOT NULL, version INTEGER NOT NULL,
    artifact_type TEXT NOT NULL CHECK (artifact_type IN ('chunk','summary','entity','relation')),
    artifact_id BLOB NOT NULL,
    surface TEXT NOT NULL CHECK (surface IN ('vector','graph')),
    generation INTEGER NOT NULL DEFAULT 1,
    idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','committed','retry_wait','orphan')),
    attempt INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, workspace_id, source_id, version, artifact_id, surface, generation)
);
CREATE INDEX oc_ledger_state ON oc_artifact_ledger(state, next_retry_at);
```

- [x] **Step 2: 更新 REQUIRED_TABLES**

在 `src/storage/sqlite.rs` 的 `REQUIRED_TABLES` 数组里 `"oc_audit"` 之后加：

```rust
const REQUIRED_TABLES: &[&str] = &[
    // ... existing entries ...
    "oc_audit",
    "oc_artifact_owners",
    "oc_index_entries",
    "oc_artifact_ledger",
];
```

- [x] **Step 3: 运行测试确认结构通过**

Run: `cargo test --locked --offline --test initialization --test sqlite_store`
Expected: PASS（新表被创建、`check()` 通过；`sqlite_initializes_once_and_rejects_incompatible` 仍验证 drop 表会被拒绝）。

- [x] **Step 4: 提交**

```bash
git add src/storage/sqlite-schema.sql src/storage/sqlite.rs
git commit -m "feat(storage): add artifact owners/index/ledger tables (M3)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 2: 账本领域类型

**Files:**
- Create: `src/storage/ledger.rs`
- Modify: `src/storage/mod.rs`（`pub mod ledger;` + re-export 类型）

**Interfaces:**
- Produces: `Surface`、`LedgerState`、`LedgerKey`、`LedgerEntry`、`MAX_LEDGER_ATTEMPTS`、`LEDGER_PENDING_TIMEOUT_MS`、`ledger_idempotency_key`。Task 3–5 消费这些。

- [x] **Step 1: 写失败测试**

Create `tests/sqlite_ledger.rs`：

```rust
//! Ledger type unit tests (M3): surface/state encoding and deterministic keys.

use opencontext::storage::ledger::{
    LedgerEntry, LedgerKey, LedgerState, Surface, ledger_idempotency_key,
};
use opencontext::storage::scope::{Scope, SourceVersion};
use uuid::Uuid;

fn key() -> LedgerKey {
    LedgerKey {
        scope: Scope {
            tenant_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        },
        source: SourceVersion {
            source_id: Uuid::new_v4(),
            version: 1,
        },
        artifact_id: Uuid::new_v4(),
        surface: Surface::Graph,
        generation: 2,
    }
}

#[test]
fn surface_and_state_encode_to_schema_tokens() {
    assert_eq!(Surface::Vector.as_str(), "vector");
    assert_eq!(Surface::Graph.as_str(), "graph");
    assert_eq!(LedgerState::Pending.as_str(), "pending");
    assert_eq!(LedgerState::Committed.as_str(), "committed");
    assert_eq!(LedgerState::RetryWait.as_str(), "retry_wait");
    assert_eq!(LedgerState::Orphan.as_str(), "orphan");
}

#[test]
fn idempotency_key_is_deterministic_and_scoped() {
    let k = key();
    let a = LedgerEntry {
        key: k,
        artifact_type: "entity".into(),
        idempotency_key: ledger_idempotency_key(&k),
    };
    let b = LedgerEntry {
        key: k,
        artifact_type: "entity".into(),
        idempotency_key: ledger_idempotency_key(&k),
    };
    assert_eq!(a.idempotency_key, b.idempotency_key);

    let mut other = k;
    other.generation = 3;
    assert_ne!(ledger_idempotency_key(&k), ledger_idempotency_key(&other));
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cargo test --locked --offline --test sqlite_ledger`
Expected: FAIL（`opencontext::storage::ledger` 模块不存在，编译失败）。

- [x] **Step 3: 实现 ledger.rs**

Create `src/storage/ledger.rs`：

```rust
//! Cross-store ledger types (M3): the relational authority for vector/graph
//! write progress (A2.6). Domain code sees only these types, never SQL.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::scope::{Scope, SourceVersion};

/// Which external surface a ledger entry tracks (A2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Surface {
    Vector,
    Graph,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Graph => "graph",
        }
    }
}

/// Lifecycle state of a cross-store write (A2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LedgerState {
    Pending,
    Committed,
    RetryWait,
    Orphan,
}

impl LedgerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::RetryWait => "retry_wait",
            Self::Orphan => "orphan",
        }
    }
}

/// Retry budget before a write is abandoned as an orphan (A2.6).
pub const MAX_LEDGER_ATTEMPTS: i32 = 5;

/// A pending write left this long without confirm is abandoned by reconcile.
pub const LEDGER_PENDING_TIMEOUT_MS: i64 = 5 * 60 * 1000;

/// The identity of a cross-store write: everything the external store needs to
/// idempotently place one artifact (A2.6). Matches the ledger primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LedgerKey {
    pub scope: Scope,
    pub source: SourceVersion,
    pub artifact_id: Uuid,
    pub surface: Surface,
    pub generation: i64,
}

/// The content recorded for a pending write (A2.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub key: LedgerKey,
    pub artifact_type: String,
    pub idempotency_key: String,
}

/// Deterministic idempotency key for a ledger write: the worker derives its
/// external-store key from this so a replay targets the same object (A2.6).
pub fn ledger_idempotency_key(key: &LedgerKey) -> String {
    format!(
        "{}-{}-{}-{}-{}-{}-{}",
        key.scope.tenant_id,
        key.scope.workspace_id,
        key.source.source_id,
        key.source.version,
        key.artifact_id,
        key.surface.as_str(),
        key.generation
    )
}
```

在 `src/storage/mod.rs` 的模块声明里加 `pub mod ledger;`（放在 `pub mod error;` 之后），并在 `pub use` 块追加：

```rust
pub use ledger::{
    LedgerEntry, LedgerKey, LedgerState, Surface, ledger_idempotency_key,
};
```

- [x] **Step 4: 运行测试确认通过**

Run: `cargo test --locked --offline --test sqlite_ledger`
Expected: PASS（2 个测试）。

- [x] **Step 5: 提交**

```bash
git add src/storage/ledger.rs src/storage/mod.rs tests/sqlite_ledger.rs
git commit -m "feat(storage): add ledger domain types (M3)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 3: 溯源登记与归属查询

**Files:**
- Modify: `src/storage/traits.rs`（`DomainTx` 加 3 个方法）
- Modify: `src/storage/sqlite.rs`（`SqliteTx` 实现）
- Modify: `tests/storage_contract.rs`（`MemTx` stub）
- Test: `tests/sqlite_owner.rs`（新建）

**Interfaces:**
- Consumes: `SourceVersion`（scope.rs）、`oc_artifact_owners`（Task 1）。
- Produces: `DomainTx::register_owner(source, artifact_type, artifact_id, chunk_id)`、`DomainTx::detach_owners(source) -> u64`、`DomainTx::owned_artifacts() -> Value`。

`owned_artifacts` 返回 JSON `{"relations": [Uuid, ...], "entities": [Uuid, ...]}`：当前仍有 owner 的 relation/entity id 列表。SQLite 只存 owner、不存实体/关系本体（图在 M4），所以 M3 **不能**单靠 SQLite 判定孤儿——「孤儿 = Kuzu 全量 id − 此列表」由 M4 的清理规划器 diff 得出（graph-core G4）。本方法只提供「谁还被归属」这一侧。

- [x] **Step 1: 在 trait 里声明方法**

在 `src/storage/traits.rs` 的 `DomainTx` trait 内（`commit`/`rollback` 之前）加：

```rust
    // -- provenance (M3) -----------------------------------------------------

    /// Record that `artifact_id` (a chunk/summary/entity/relation) originates
    /// from `source`. `chunk_id` is set only for chunk/summary artifacts.
    async fn register_owner(
        &mut self,
        source: SourceVersion,
        artifact_type: &str,
        artifact_id: Uuid,
        chunk_id: Option<Uuid>,
    ) -> StorageResult<()>;

    /// Remove every owner row for `source`, returning rows removed. Called
    /// when a source is retracted so derived artifacts can be reaped.
    async fn detach_owners(&mut self, source: SourceVersion) -> StorageResult<u64>;

    /// The currently-owned entity/relation ids, as
    /// `{"relations": [..], "entities": [..]}`. The graph adapter (M4) diffs
    /// this against its own rows to find orphans (graph-core G4); artifacts
    /// shared with another source stay in this set.
    async fn owned_artifacts(&mut self) -> StorageResult<Value>;
```

- [x] **Step 2: 写失败测试**

Create `tests/sqlite_owner.rs`：

```rust
//! Provenance owner tests (M3): register, shared owners, detach, ownership.

use opencontext::storage::{
    AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion,
    sqlite::SqliteStore,
};
use uuid::Uuid;

async fn provision(store: &SqliteStore) -> AuthorizedScope {
    let tenant = Uuid::new_v4();
    let workspace = Uuid::new_v4();
    store.create_workspace(tenant, workspace, "acme").await.unwrap();
    let scope = Scope { tenant_id: tenant, workspace_id: workspace };
    let key = store.issue_key(scope, "admin").await.unwrap();
    store.authenticate(&key.token).await.unwrap()
}

async fn begin_write(store: &SqliteStore, auth: &AuthorizedScope) -> opencontext::storage::sqlite::SqliteTx {
    let mut tx = store.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx
}

#[tokio::test]
async fn detaching_a_source_drops_its_sole_artifacts_but_retains_shared() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    let s1 = SourceVersion { source_id: Uuid::new_v4(), version: 1 };
    let s2 = SourceVersion { source_id: Uuid::new_v4(), version: 1 };
    let shared = Uuid::new_v4();
    let sole = Uuid::new_v4();

    // s1 and s2 both own `shared`; only s1 owns `sole`.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_owner(s1, "entity", shared, None).await.unwrap();
        tx.register_owner(s1, "entity", sole, None).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_owner(s2, "entity", shared, None).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Detaching s1 removes exactly its two owner rows.
    let removed = {
        let mut tx = begin_write(&store, &auth).await;
        tx.detach_owners(s1).await.unwrap()
    };
    assert_eq!(removed, 2);

    // `shared` is still owned (by s2) and stays; `sole` has no owner left and
    // is gone from the owned set (M4 will diff this against Kuzu to reap it).
    let owned = {
        let mut tx = begin_write(&store, &auth).await;
        tx.owned_artifacts().await.unwrap()
    };
    let entities: Vec<String> = owned["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(entities.contains(&shared.to_string()));
    assert!(!entities.contains(&sole.to_string()));
}
```

- [x] **Step 3: 运行测试确认失败**

Run: `cargo test --locked --offline --test sqlite_owner`
Expected: FAIL（`DomainTx` 无 `register_owner` 方法，编译失败）。

- [x] **Step 4: 实现 SqliteTx 的三个方法**

在 `src/storage/sqlite.rs` 的 `impl DomainTx for SqliteTx` 内（`cleanup_derived_chunks` 之后）加：

```rust
    async fn register_owner(
        &mut self,
        source: SourceVersion,
        artifact_type: &str,
        artifact_id: Uuid,
        chunk_id: Option<Uuid>,
    ) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_artifact_owners(tenant_id, workspace_id, source_id, version, \
             artifact_type, artifact_id, chunk_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(source.source_id)
        .bind(source.version)
        .bind(artifact_type)
        .bind(artifact_id)
        .bind(chunk_id)
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn detach_owners(&mut self, source: SourceVersion) -> StorageResult<u64> {
        let res = sqlx::query(
            "DELETE FROM oc_artifact_owners \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ?",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .bind(source.source_id)
        .bind(source.version)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(res.rows_affected())
    }

    async fn owned_artifacts(&mut self) -> StorageResult<Value> {
        // Return the current owner set grouped by artifact. M4 diffs this
        // against its Kuzu rows to find orphans (graph-core G4); M3 itself does
        // not store entity/relation bodies.
        let owned: Vec<(String, Uuid)> = sqlx::query_as(
            "SELECT artifact_type, artifact_id FROM oc_artifact_owners \
             WHERE tenant_id = ? AND workspace_id = ? \
               AND artifact_type IN ('entity','relation')",
        )
        .bind(self.scope.tenant_id)
        .bind(self.scope.workspace_id)
        .fetch_all(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let mut relations = Vec::new();
        let mut entities = Vec::new();
        for (kind, id) in owned {
            match kind.as_str() {
                "relation" => relations.push(id),
                "entity" => entities.push(id),
                _ => {}
            }
        }
        Ok(json!({ "relations": relations, "entities": entities }))
    }
```

- [x] **Step 5: 给 MemTx 加 stub**

在 `tests/storage_contract.rs` 的 `impl DomainTx for MemTx` 内（`record_source` 之后）加：

```rust
    async fn register_owner(
        &mut self,
        _source: SourceVersion,
        _artifact_type: &str,
        _artifact_id: Uuid,
        _chunk_id: Option<Uuid>,
    ) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }

    async fn detach_owners(&mut self, _source: SourceVersion) -> StorageResult<u64> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }

    async fn owned_artifacts(&mut self) -> StorageResult<Value> {
        Err(StorageError::Unavailable(
            "in-memory double has no owner rows".into(),
        ))
    }
```

确认 `SourceVersion`、`Uuid`、`Value`、`StorageError` 已在 `storage_contract.rs` 顶部导入（该文件已 `use opencontext::storage::{...}`，需含 `SourceVersion`）。

- [x] **Step 6: 运行测试确认通过**

Run: `cargo test --locked --offline --test sqlite_owner --test storage_contract`
Expected: PASS。

- [x] **Step 7: 提交**

```bash
git add src/storage/traits.rs src/storage/sqlite.rs tests/storage_contract.rs tests/sqlite_owner.rs
git commit -m "feat(storage): add provenance owner registration and ownership query (M3)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 4: 账本状态机（pending/committed/retryable/permanent）

**Files:**
- Modify: `src/storage/traits.rs`（`DomainTx` 加 4 个方法）
- Modify: `src/storage/sqlite.rs`（`SqliteTx` 实现）
- Modify: `tests/storage_contract.rs`（`MemTx` stub）
- Test: `tests/sqlite_ledger.rs`（追加测试）

**Interfaces:**
- Consumes: `LedgerEntry`、`LedgerKey`、`Surface`、`MAX_LEDGER_ATTEMPTS`（ledger.rs）；三张表（Task 1）。
- Produces: `DomainTx::register_pending(entry)`、`DomainTx::confirm_committed(key)`、`DomainTx::fail_retryable(key, error)`、`DomainTx::fail_permanent(key, error)`。

语义（A2.6）：
- `register_pending`：`INSERT ... ON CONFLICT DO NOTHING`，state 默认 `pending`。
- `confirm_committed`：`UPDATE ... SET state='committed'`，但**同一语句内复核来源有效性**（`oc_events.state='active'` 的 `EXISTS` 子查询）并匹配完整 PK（generation 随之复核）。来源已撤回或 generation 已变 → 0 行 → 幂等 no-op，由 Worker 改走 `fail_permanent` 降级（A2.6 第 3 步「复核来源/generation」）。「取消」复核复用既有 `active_job`/`settle` 机制，属 Worker 编排（M5），本方法不重复实现。
- `fail_retryable`：`attempt+1`；达到 `MAX_LEDGER_ATTEMPTS` 置 `orphan`，否则置 `retry_wait` 并写 `next_retry_at`（`retry_delay(attempt)` 指数退避）、`last_error`。
- `fail_permanent`：置 `orphan`、写 `last_error`。

- [x] **Step 1: 在 trait 里声明方法**

在 `src/storage/traits.rs` 的 `DomainTx` trait 内（Task 3 方法之后）加：

```rust
    // -- cross-store ledger (M3) ---------------------------------------------

    /// Register a pending cross-store write on this transaction (A2.6 step 1).
    /// Idempotent: a repeated key leaves the existing row untouched.
    async fn register_pending(&mut self, entry: LedgerEntry) -> StorageResult<()>;

    /// Confirm a pending write committed, re-checking source validity and
    /// generation in the same statement (A2.6 step 3). No-op (idempotent) if
    /// the entry is already committed, the source is retracted, or its
    /// generation changed.
    async fn confirm_committed(&mut self, key: LedgerKey) -> StorageResult<()>;

    /// Record a retryable external-write failure; becomes `orphan` after the
    /// retry budget is exhausted (A2.6).
    async fn fail_retryable(&mut self, key: LedgerKey, error: &str) -> StorageResult<()>;

    /// Record a permanent external-write failure as `orphan` (A2.6).
    async fn fail_permanent(&mut self, key: LedgerKey, error: &str) -> StorageResult<()>;
```

同时在该文件顶部补一行 `use crate::storage::ledger::{LedgerEntry, LedgerKey};`（加在 `use crate::storage::{...}` 那组之后）。

- [x] **Step 2: 写失败测试**

在 `tests/sqlite_ledger.rs` 追加：

```rust
//! (continuation) ledger state-machine tests against a real SQLite file.

use opencontext::storage::ledger::{LedgerEntry, LedgerKey, Surface, MAX_LEDGER_ATTEMPTS};
use opencontext::storage::{AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion, sqlite::SqliteStore};

async fn provision(store: &SqliteStore) -> AuthorizedScope {
    let tenant = uuid::Uuid::new_v4();
    let workspace = uuid::Uuid::new_v4();
    store.create_workspace(tenant, workspace, "acme").await.unwrap();
    let scope = Scope { tenant_id: tenant, workspace_id: workspace };
    let key = store.issue_key(scope, "admin").await.unwrap();
    store.authenticate(&key.token).await.unwrap()
}

async fn begin_write(store: &SqliteStore, auth: &AuthorizedScope) -> opencontext::storage::sqlite::SqliteTx {
    let mut tx = store.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx
}

/// Create a live `active` source event and return its id, so `confirm_committed`
/// has a valid source to re-check (A2.6 step 3).
async fn create_active_event(store: &SqliteStore, auth: &AuthorizedScope) -> uuid::Uuid {
    let mut tx = begin_write(store, auth).await;
    let id = tx.create_event("memory", "hello", None).await.unwrap();
    tx.commit().await.unwrap();
    id
}

fn entry(scope: Scope, source_id: uuid::Uuid, artifact_id: uuid::Uuid) -> LedgerEntry {
    LedgerEntry {
        key: LedgerKey {
            scope,
            source: SourceVersion { source_id, version: 1 },
            artifact_id,
            surface: Surface::Vector,
            generation: 1,
        },
        artifact_type: "chunk".into(),
        idempotency_key: "k".into(),
    }
}

async fn ledger_state(store: &SqliteStore, key: &LedgerKey) -> String {
    sqlx::query_scalar(
        "SELECT state FROM oc_artifact_ledger \
         WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
           AND artifact_id = ? AND surface = ? AND generation = ?",
    )
    .bind(key.scope.tenant_id)
    .bind(key.scope.workspace_id)
    .bind(key.source.source_id)
    .bind(key.source.version)
    .bind(key.artifact_id)
    .bind(key.surface.as_str())
    .bind(key.generation)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn pending_confirms_committed_when_source_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    let source_id = create_active_event(&store, &auth).await;
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "pending");

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "committed");
}

#[tokio::test]
async fn confirm_is_a_noop_when_source_is_retracted() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    let source_id = create_active_event(&store, &auth).await;
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Retract the source, then confirm must NOT publish.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.retract_event(source_id).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "pending");
}

#[tokio::test]
async fn retryable_then_permanent_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;
    // fail_retryable/fail_permanent do not re-check the source, so a random
    // (non-existent) source id is fine here.
    let e = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // First failure is retryable.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.fail_retryable(e.key, "boom").await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "retry_wait");

    // Exhaust the budget to orphan.
    for _ in 0..MAX_LEDGER_ATTEMPTS {
        let mut tx = begin_write(&store, &auth).await;
        tx.fail_retryable(e.key, "boom").await.unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(ledger_state(&store, &e.key).await, "orphan");
}
```

- [x] **Step 3: 运行测试确认失败**

Run: `cargo test --locked --offline --test sqlite_ledger`
Expected: FAIL（`DomainTx` 无 `register_pending` 等方法，编译失败）。

- [x] **Step 4: 实现 SqliteTx 的四个方法**

在 `src/storage/sqlite.rs` 的 `impl DomainTx for SqliteTx` 内加：

```rust
    async fn register_pending(&mut self, entry: LedgerEntry) -> StorageResult<()> {
        sqlx::query(
            "INSERT INTO oc_artifact_ledger(tenant_id, workspace_id, source_id, version, \
             artifact_type, artifact_id, surface, generation, idempotency_key, state, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(entry.key.scope.tenant_id)
        .bind(entry.key.scope.workspace_id)
        .bind(entry.key.source.source_id)
        .bind(entry.key.source.version)
        .bind(entry.artifact_type)
        .bind(entry.key.artifact_id)
        .bind(entry.key.surface.as_str())
        .bind(entry.key.generation)
        .bind(entry.idempotency_key)
        .bind(now_ms())
        .bind(now_ms())
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn confirm_committed(&mut self, key: LedgerKey) -> StorageResult<()> {
        // Re-check source validity and generation in the same statement (A2.6
        // step 3): only an active source's matching generation commits.
        sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'committed', updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ? \
               AND state IN ('pending','retry_wait') \
               AND EXISTS (SELECT 1 FROM oc_events e \
                           WHERE e.tenant_id = oc_artifact_ledger.tenant_id \
                             AND e.workspace_id = oc_artifact_ledger.workspace_id \
                             AND e.id = oc_artifact_ledger.source_id \
                             AND e.state = 'active')",
        )
        .bind(now_ms())
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }

    async fn fail_retryable(&mut self, key: LedgerKey, error: &str) -> StorageResult<()> {
        let now = now_ms();
        let attempt: i32 = sqlx::query_scalar(
            "SELECT attempt FROM oc_artifact_ledger \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ?",
        )
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .fetch_one(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        let next = attempt + 1;
        if next >= MAX_LEDGER_ATTEMPTS {
            sqlx::query(
                "UPDATE oc_artifact_ledger SET state = 'orphan', attempt = ?, last_error = ?, \
                 next_retry_at = NULL, updated_at = ? \
                 WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
                   AND artifact_id = ? AND surface = ? AND generation = ? \
                   AND state IN ('pending','retry_wait')",
            )
            .bind(next)
            .bind(error)
            .bind(now)
            .bind(key.scope.tenant_id)
            .bind(key.scope.workspace_id)
            .bind(key.source.source_id)
            .bind(key.source.version)
            .bind(key.artifact_id)
            .bind(key.surface.as_str())
            .bind(key.generation)
            .execute(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
        } else {
            sqlx::query(
                "UPDATE oc_artifact_ledger SET state = 'retry_wait', attempt = ?, last_error = ?, \
                 next_retry_at = ?, updated_at = ? \
                 WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
                   AND artifact_id = ? AND surface = ? AND generation = ? \
                   AND state IN ('pending','retry_wait')",
            )
            .bind(next)
            .bind(error)
            .bind(now + retry_delay(next))
            .bind(now)
            .bind(key.scope.tenant_id)
            .bind(key.scope.workspace_id)
            .bind(key.source.source_id)
            .bind(key.source.version)
            .bind(key.artifact_id)
            .bind(key.surface.as_str())
            .bind(key.generation)
            .execute(&mut *self.tx)
            .await
            .map_err(sqlite_err)?;
        }
        Ok(())
    }

    async fn fail_permanent(&mut self, key: LedgerKey, error: &str) -> StorageResult<()> {
        sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'orphan', last_error = ?, updated_at = ? \
             WHERE tenant_id = ? AND workspace_id = ? AND source_id = ? AND version = ? \
               AND artifact_id = ? AND surface = ? AND generation = ? \
               AND state IN ('pending','retry_wait')",
        )
        .bind(error)
        .bind(now_ms())
        .bind(key.scope.tenant_id)
        .bind(key.scope.workspace_id)
        .bind(key.source.source_id)
        .bind(key.source.version)
        .bind(key.artifact_id)
        .bind(key.surface.as_str())
        .bind(key.generation)
        .execute(&mut *self.tx)
        .await
        .map_err(sqlite_err)?;
        Ok(())
    }
```

在 `src/storage/sqlite.rs` 顶部 `use crate::storage::traits::{...}` 之外补 `use crate::storage::ledger::{LedgerEntry, LedgerKey, MAX_LEDGER_ATTEMPTS};`。

- [x] **Step 5: 给 MemTx 加 stub**

在 `tests/storage_contract.rs` 的 `impl DomainTx for MemTx` 内加：

```rust
    async fn register_pending(&mut self, _entry: LedgerEntry) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn confirm_committed(&mut self, _key: LedgerKey) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn fail_retryable(&mut self, _key: LedgerKey, _error: &str) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }

    async fn fail_permanent(&mut self, _key: LedgerKey, _error: &str) -> StorageResult<()> {
        Err(StorageError::Unavailable(
            "in-memory double has no ledger".into(),
        ))
    }
```

补 `use opencontext::storage::ledger::{LedgerEntry, LedgerKey};`。

- [x] **Step 6: 运行测试确认通过**

Run: `cargo test --locked --offline --test sqlite_ledger --test storage_contract`
Expected: PASS。

- [x] **Step 7: 提交**

```bash
git add src/storage/traits.rs src/storage/sqlite.rs tests/storage_contract.rs tests/sqlite_ledger.rs
git commit -m "feat(storage): add cross-store ledger state machine (M3)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 5: 对账重放

**Files:**
- Modify: `src/storage/traits.rs`（`RelationalStore` 加 `reconcile_ledger`）
- Modify: `src/storage/sqlite.rs`（`SqliteStore` 实现）
- Modify: `tests/storage_contract.rs`（`MemRelationalStore` stub）
- Test: `tests/storage_recovery.rs`（新建）

**Interfaces:**
- Consumes: `LedgerState`、`MAX_LEDGER_ATTEMPTS`、`LEDGER_PENDING_TIMEOUT_MS`（ledger.rs）；`oc_artifact_ledger`（Task 1）。
- Produces: `RelationalStore::reconcile_ledger() -> Value`，返回 `{"requeued": n, "orphaned": n}`。

语义（A2.6「重启重放」）：把「卡在 pending 超过 `LEDGER_PENDING_TIMEOUT_MS`」的行标 orphan（来源不可知/外部写状态不明，视为放弃），把「retry_wait 且 `next_retry_at <= now`」的行标回 pending（重试，generation 不变）。不做实际外部写——真正的收敛由 M4 适配器在重放时按 idempotency_key 幂等重写。

- [x] **Step 1: 在 trait 里声明方法**

在 `src/storage/traits.rs` 的 `RelationalStore` trait 内（`revoke_key` 之后）加：

```rust
    /// Reconcile the cross-store ledger on startup/recovery: abandon stuck
    /// pending writes as orphan and requeue due retry_wait writes (A2.6).
    /// Returns counts as `{"requeued": n, "orphaned": n}`.
    async fn reconcile_ledger(&self) -> StorageResult<Value>;
```

- [x] **Step 2: 写失败测试**

Create `tests/storage_recovery.rs`：

```rust
//! Ledger reconcile tests (M3): stuck pending and due retry_wait converge.

use opencontext::storage::ledger::{LedgerEntry, LedgerKey, Surface};
use opencontext::storage::{AuthorizedScope, DomainTx, Permission, RelationalStore, Scope, SourceVersion, sqlite::SqliteStore};

async fn provision(store: &SqliteStore) -> AuthorizedScope {
    let tenant = uuid::Uuid::new_v4();
    let workspace = uuid::Uuid::new_v4();
    store.create_workspace(tenant, workspace, "acme").await.unwrap();
    let scope = Scope { tenant_id: tenant, workspace_id: workspace };
    let key = store.issue_key(scope, "admin").await.unwrap();
    store.authenticate(&key.token).await.unwrap()
}

async fn begin_write(store: &SqliteStore, auth: &AuthorizedScope) -> opencontext::storage::sqlite::SqliteTx {
    let mut tx = store.begin(auth.clone()).await.unwrap();
    tx.check_permission(Permission::Write).await.unwrap();
    tx
}

fn entry(scope: Scope, source_id: uuid::Uuid, artifact_id: uuid::Uuid) -> LedgerEntry {
    LedgerEntry {
        key: LedgerKey {
            scope,
            source: SourceVersion { source_id, version: 1 },
            artifact_id,
            surface: Surface::Vector,
            generation: 1,
        },
        artifact_type: "chunk".into(),
        idempotency_key: "k".into(),
    }
}

#[tokio::test]
async fn reconcile_orphans_stuck_pending_and_requeues_due_retry_wait() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    let stuck = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let due = entry(auth.scope, uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(stuck.clone()).await.unwrap();
        tx.register_pending(due.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Make `stuck` look abandoned and `due` already past its retry time.
    sqlx::query(
        "UPDATE oc_artifact_ledger SET state = 'retry_wait', next_retry_at = 0, attempt = 1 \
         WHERE artifact_id = ?",
    )
    .bind(due.key.artifact_id)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE oc_artifact_ledger SET updated_at = 0 WHERE artifact_id = ?",
    )
    .bind(stuck.key.artifact_id)
    .execute(store.pool())
    .await
    .unwrap();

    let report = store.reconcile_ledger().await.unwrap();
    assert_eq!(report["orphaned"].as_i64().unwrap(), 1);
    assert_eq!(report["requeued"].as_i64().unwrap(), 1);

    let stuck_state: String = sqlx::query_scalar(
        "SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?",
    )
    .bind(stuck.key.artifact_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(stuck_state, "orphan");

    let due_state: String = sqlx::query_scalar(
        "SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?",
    )
    .bind(due.key.artifact_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(due_state, "pending");
}
```

- [x] **Step 3: 运行测试确认失败**

Run: `cargo test --locked --offline --test storage_recovery`
Expected: FAIL（`RelationalStore` 无 `reconcile_ledger`，编译失败）。

- [x] **Step 4: 实现 reconcile_ledger**

在 `src/storage/sqlite.rs` 的 `impl RelationalStore for SqliteStore` 内（`revoke_key` 之后）加：

```rust
    async fn reconcile_ledger(&self) -> StorageResult<Value> {
        let now = now_ms();
        let orphaned = sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'orphan', updated_at = ? \
             WHERE state = 'pending' AND updated_at < ?",
        )
        .bind(now)
        .bind(now - LEDGER_PENDING_TIMEOUT_MS)
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?
        .rows_affected();
        let requeued = sqlx::query(
            "UPDATE oc_artifact_ledger SET state = 'pending', updated_at = ? \
             WHERE state = 'retry_wait' AND next_retry_at IS NOT NULL AND next_retry_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(sqlite_err)?
        .rows_affected();
        Ok(json!({ "requeued": requeued, "orphaned": orphaned }))
    }
```

在 `src/storage/sqlite.rs` 顶部补 `use crate::storage::ledger::LEDGER_PENDING_TIMEOUT_MS;`。

- [x] **Step 5: 给 MemRelationalStore 加 stub**

在 `tests/storage_contract.rs` 的 `impl RelationalStore for MemRelationalStore` 内加：

```rust
    async fn reconcile_ledger(&self) -> StorageResult<Value> {
        Ok(serde_json::json!({"requeued": 0, "orphaned": 0}))
    }
```

- [x] **Step 6: 运行测试确认通过**

Run: `cargo test --locked --offline --test storage_recovery --test storage_contract`
Expected: PASS。

- [x] **Step 7: 提交**

```bash
git add src/storage/traits.rs src/storage/sqlite.rs tests/storage_contract.rs tests/storage_recovery.rs
git commit -m "feat(storage): add ledger reconcile on startup (M3)

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

### Task 6: 端到端恢复与全量验证

**Files:**
- Modify: `docs/superpowers/plans/2026-09-22-pluggable-storage-engine.md`（M3 勾选）
- Modify: `docs/STATUS.md`（M3 状态 + 下一阶段 M4）
- Test: `tests/storage_recovery.rs`（追加端到端收敛测试）

**Interfaces:**
- Consumes: Task 3–5 的全部方法。

- [x] **Step 1: 写端到端收敛测试**

在 `tests/storage_recovery.rs` 追加一个验证「确认幂等 + 来源复核通过才发布」的测试：

```rust
#[tokio::test]
async fn confirm_is_idempotent_and_republishes_nothing_twice() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("oc.db")).await.unwrap();
    let auth = provision(&store).await;

    // A live source so confirm's source re-check passes (A2.6 step 3).
    let source_id = {
        let mut tx = begin_write(&store, &auth).await;
        let id = tx.create_event("memory", "hello", None).await.unwrap();
        tx.commit().await.unwrap();
        id
    };
    let e = entry(auth.scope, source_id, uuid::Uuid::new_v4());

    {
        let mut tx = begin_write(&store, &auth).await;
        tx.register_pending(e.clone()).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Confirm twice: the second is a no-op, not an error.
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = begin_write(&store, &auth).await;
        tx.confirm_committed(e.key).await.unwrap();
        tx.commit().await.unwrap();
    }

    let state: String = sqlx::query_scalar(
        "SELECT state FROM oc_artifact_ledger WHERE artifact_id = ?",
    )
    .bind(e.key.artifact_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(state, "committed");
}
```

- [x] **Step 2: 运行全量验证**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo test --locked --offline
```
Expected: 全部 PASS（含 `sqlite_ledger`、`sqlite_owner`、`storage_recovery` 新套件，以及既有 `sqlite_store`/`sqlite_domain`/`sqlite_lock`/`storage_contract`/`initialization`）。

- [x] **Step 3: 更新计划勾选与 STATUS**

在 `docs/superpowers/plans/2026-09-22-pluggable-storage-engine.md` 的 M3 节，把五条勾选全部置为 `[x]`（前两条：三张表 + pending/confirm（含来源/generation 复核）；第三/四条：retry_wait/orphan/对账 + 删除墓碑/依赖顺序清理——其中「依赖顺序清理」的图/向量物理删除与孤儿 diff 属 M4，M3 交付 owner detach + 归属查询；第五条：崩溃/失败/迟到/取消/共享删除测试）。

在 `docs/STATUS.md` 把「下一阶段是 M3」改为「下一阶段是 M4 LanceDB/Kuzu 与检索」，并简述 M3 已交付账本/溯源/对账。

- [x] **Step 4: 提交**

```bash
git add tests/storage_recovery.rs docs/superpowers/plans/2026-09-22-pluggable-storage-engine.md docs/STATUS.md
git commit -m "test(storage): add ledger reconcile e2e and mark M3 done

Co-Authored-By: Claude Code <noreply@anthropic.com>"
```

---

## Self-Review 记录

- **Spec 覆盖**：A2.6 的四步流程 → Task 4（登记/确认含来源复核/失败）+ Task 5（对账）+ Task 3（溯源/归属查询）；M3 计划五条 → Task 1/2/3/4/5/6；「删除先墓碑后按依赖顺序清理」中 SQLite 侧的 owner detach + 归属查询在 Task 3，孤儿 diff 与图/向量物理删除留给 M4（graph-core G4）；「取消」复核复用既有 `active_job`/`settle`（M5 编排）。
- **占位符扫描**：无 TBD/TODO；所有代码步骤含完整 SQL/Rust/测试。
- **类型一致性**：`LedgerKey`/`LedgerEntry`/`Surface`/`LedgerState` 在 Task 2 定义，Task 4/5 一致引用；`SourceVersion` 复用 scope.rs；`register_owner` 等签名在 Task 3/4 声明一次、三处实现（trait/SqliteTx/MemTx）一致；`confirm_committed` 的来源复核依赖 `oc_events.state`（schema 已确认 `active`/`retracted`）、`fail_retryable` 复用已有 `retry_delay`（sqlite.rs:307）。
