# openContext / ContextDB

用 Rust 实现的团队 Agent 记忆与知识服务。数据归属与权限边界是 workspace：结构化授权输入可直接发布，自动抽取只生成候选；检索只返回已发布且来源仍有效的版本。

当前为可运行的首版原型（HTTP API、独立 Apalis Worker、PostgreSQL/pgvector、本地共享文件、只读 MCP stdio），不承诺生产 SLO、竞品效果排名或完整企业身份集成。

权威设计文档为 [记忆 + 知识库平台设计](docs/superpowers/specs/2026-09-22-memory-knowledge-platform-design.md)。该 spec 描述平台**目标架构**（知识图谱、Cognee 式自动发布、可插拔存储引擎、会话记忆与经验蒸馏、多租户 SaaS），分 P0/P1/P2 三期；当前代码处于首版基线，spec 中多数能力尚未实现，详见文末 [演进路线](#演进路线)。

## Cognee 技术调研

调研固定到 Cognee 1.6.0 提交 `663a2dc15d04bc0d7ec2733a2dd604b7ed1b8c8e`，建议按顺序阅读：

1. [技术架构分析](docs/Cognee_技术架构分析.md)：组件、知识加工、检索、会话记忆与删除恢复，及复刻边界。
2. [技术细节与方案对比](docs/Cognee_技术细节与方案对比.md)：数据契约、身份去重、排序、反馈蒸馏、竞品机制与验收。
3. [配图与核验材料](docs/assets/cognee/README.md)：8 张流程图、图源、生成脚本、源码清单与数值核验。

以上为源码分析与建议；本项目尚未集成 Cognee，也未运行完整服务或竞品对比。

## 快速启动

需要 Docker Compose。复制 `.env.example` 为 `.env`，为 `POSTGRES_PASSWORD` 和 `OC_RUNTIME_PASSWORD` 各填一个至少 24 字符的随机十六进制密码（十六进制避免 URL 转义）。不要提交 `.env`。

```sh
docker compose up --build -d
docker compose run --rm admin workspace-create demo
```

返回 workspace ID 和一次性展示的 admin token（数据库只存其 SHA-256）。保存 token，再用最小权限凭据接入 Agent：

```sh
docker compose run --rm admin key-create WORKSPACE_UUID --role reader
docker compose run --rm admin key-create WORKSPACE_UUID --role writer
docker compose run --rm admin key-revoke KEY_UUID
curl http://127.0.0.1:8080/health/ready
```

数据库不暴露主机端口，API 仅绑定回环。API 与 Worker 用非管理员 DB 角色和同一 Files 卷。跨主机部署需共享文件服务；对外开放 HTTP 前由平台配置 TLS 与访问控制。

## 第一个闭环

将 admin token 放入当前 shell 的 `OC_API_KEY`，不要写入代码。所有修改请求须带 `Idempotency-Key`。

```sh
curl -sS http://127.0.0.1:8080/v1/memories \
  -H "Authorization: Bearer $OC_API_KEY" \
  -H 'Idempotency-Key: release-rule-1' -H 'Content-Type: application/json' \
  -d '{"fact_key":"release.policy","content":"生产发布必须经过审批","publish_if_authorized":true}'
```

响应含 `asset_id`、`source_event_id`、`candidate_id`、`job_id`。此时仅受理；轮询任务到 `state=completed` 且 `outcome=published`，再读取或检索：

```sh
curl -sS http://127.0.0.1:8080/v1/jobs/JOB_UUID -H "Authorization: Bearer $OC_API_KEY"
curl -sS http://127.0.0.1:8080/v1/search \
  -H "Authorization: Bearer $OC_API_KEY" -H 'Content-Type: application/json' \
  -d '{"query":"发布审批","mode":"keyword","limit":10}'
```

writer 即使带 `publish_if_authorized=true` 也只能创建候选；已有事实的后续输入一律经 review，不能隐式覆盖。审核需带候选 `expected_revision`、资产 `expected_version` 和原因。恢复通过追加版本完成；删除资产后不可恢复，撤回来源后不可恢复引用该来源的版本。

完整接口、状态与错误约定见 [API 文档](docs/API.md)。

## 从源码运行

已验证 Rust 1.96.0（最低声明 1.88，未在最低版本测试）。PostgreSQL 17 需 pgvector。初始化管理员需建扩展/角色/表权限；认证函数所有者需能绕过 API key 表的 RLS（本地用 PostgreSQL 管理员）。

```sh
cargo build --locked
# DATABASE_URL 指向管理员连接
# OC_RUNTIME_PASSWORD 至少 24 字符
cargo run --locked -- runtime-setup
cargo run --locked -- workspace-create demo
# 改 DATABASE_URL 为 oc_runtime 连接；两个终端分别运行
cargo run --locked -- api
cargo run --locked -- worker
```

CLI 启动 API/Worker/MCP 时拒绝 superuser、BYPASSRLS、表所有者及其成员。每个业务事务重新核验 key 并设置 transaction-local tenant/workspace。运行角色是可信应用凭据，不可交给 Agent；RLS 不防御持有该凭据且可任意设置会话变量的攻击者。

连接数据库时自动初始化空库：应用与 Apalis 使用当前完整结构，不保留升级脚本或版本记录。PostgreSQL 首次连接需要管理员权限，Compose 的 `runtime-setup` 自动完成初始化和运行角色授权；API/Worker/MCP 继续使用受限角色。已有库只检查必要结构，不自动升级或修复。

## MCP

MCP 客户端启动 `opencontext mcp`，通过环境传入运行角色 `DATABASE_URL`、共享 `OC_FILES_DIR`、workspace 的 `OC_API_KEY`。提供只读工具 `context_search`、`context_get`、`context_resolve`；每次调用重新认证，撤销 key 后已有进程的后续调用也失败。日志写 stderr，stdout 仅用于协议。

## 可选模型

默认 `OC_ENABLE_MODELS=false`，不调用外部模型。启用前自行明确数据出域与费用授权，为 API 和 Worker 配置相同参数：

| 变量 | 含义 |
| --- | --- |
| `OC_MODEL_BASE_URL` | 兼容 `/embeddings`、`/chat/completions` 的服务根地址，通常以 `/v1` 结尾 |
| `OC_MODEL_API_KEY` | 服务凭据 |
| `OC_EMBEDDING_MODEL` / `OC_EMBEDDING_DIMENSION` | 向量模型及维度（1–4096），可不配 |
| `OC_EXTRACTION_MODEL` | 事实抽取模型，可不配 |

远程地址要求 HTTPS，仅 localhost 允许 HTTP。禁用 HTTP 重定向，调用超时 45 秒，响应流上限 8 MB。自动抽取最多 20 个候选，模型返回发布标记也不会直接发布。未启用抽取时 capture 保留来源、任务明确失败（可用结构化 memories 入口）。

发布任务固定接收时的 embedding profile；Worker 配置不匹配会失败，不静默降级为纯关键词。模型/解析失败需显式重试，DB 错误由 Apalis 最多重试 5 次。模型调用可能因崩溃恢复重复发生；发布保持幂等，不保证供应商费用恰好一次。

## 测试

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# 需独立测试数据库，先 runtime-setup（自动初始化），再设置：
# TEST_ADMIN_DATABASE_URL / TEST_DATABASE_URL=oc_runtime 测试连接
cargo test --locked --test initialization -- --ignored
cargo test --locked --test lifecycle -- --ignored
cargo test --locked --test processes -- --ignored
```

普通 `cargo test` 跳过需 DB 的测试。initialization 用管理员创建并销毁临时数据库（需要 CREATE DATABASE 权限），验证空库、并发启动和不兼容结构拒绝。lifecycle/processes 各建独立 workspace，按顺序运行，期间不能启动其他 Worker。processes 启动真实 API/Worker/MCP/PDF 子进程及本地模拟模型（不使用付费模型），含强退恢复与处理中删除。记录见 [VALIDATION](docs/VALIDATION.md)，后续见 [STATUS](docs/STATUS.md)。

## 当前边界

- Apalis 0.7.4 的启动恢复/确认行为尚不适合多并发 Worker；DB 会话锁强制每库一个 Worker 进程，连接丢失即停止。扩容前需修复上游并验证，勿删此保护。
- 向量检索为带 workspace/profile 过滤的精确扫描；混合检索为 RRF（k=60），无 ANN、重排或自动索引重建。改模型不回填旧向量。
- `budget_tokens` 实为 UTF-8 字节数的保守预算（含标题和引用标记），非某模型精确 token 数；片段整段保留或丢弃。
- 文本/Markdown 上限 1 MB，文件 10 MiB；PDF 最多 200 页、总文本 1 MB、子进程超时 30 秒，无 OCR；任一页少于 3 个字母/数字字符即拒绝整份（含纯空白页）。PDF 子进程共享 OS 身份，非安全沙箱；Compose 仅限制 Worker 容器整体内存。
- 删除先写墓碑并撤回来源，再清理派生索引；原始文件、事件、候选正文、版本及审计暂保留，不提供原文物理擦除或按天自动清理，保留期待定。异常上传可能留下未引用文件，暂需运维按元数据核对。
- API key 为 workspace 级角色；尚无 OIDC/SSO、文档级 ACL、跨 workspace 共享、审计查询 API、分页游标、配额、公平调度或企业备份恢复编排。当前实现不能据此声称满足生产合规要求。

## 演进路线

[平台设计 spec](docs/superpowers/specs/2026-09-22-memory-knowledge-platform-design.md) 按 P0/P1/P2 分期演进。当前代码处于首版基线，下列能力**尚未实现**，README 其余章节描述的是当前已构建系统：

| spec 阶段 | 目标能力 | 当前状态 |
| --- | --- | --- |
| **P0** | 普通文本入库、稳定分块、keyword/vector 检索、自动发布、溯源+撤回、作业重试、workspace/tenant 隔离；知识图谱（实体/关系抽取、图存储、图混合检索、来源级联删除） | 文本/PDF 入库、检索、溯源撤回、重试、隔离**已实现**；但发布仍走**候选审核门**（非 spec 的自动发布），知识图谱基础写入/检索**已实现**，删除与可见性待完善 |
| **P0** | 可插拔存储引擎（`GraphStore`/`VectorStore`/`BlobStore` trait + 能力声明 + 账本对账）、对象存储抽象 | **接口尚未实现**；目标本地固定 SQLite/LanceDB/Kuzu，PG/pgvector 仅保留未来接口扩展能力；当前代码为 PG 直连 + 本地 Files |
| **P1** | 会话记忆、指导、反馈、经验蒸馏、阶段化 improve | **未实现** |
| **P2** | GraphCompletion、个性化、OIDC、配额计费、Neo4j/Qdrant 生产适配、ANN/重排 | **未实现** |

存储与自动初始化以 [整体设计 A2：存储与初始化](docs/superpowers/specs/2026-09-22-memory-knowledge-platform-design.md#storage-design) 为准。本次只实现取消版本化升级后的自动初始化，SQLite/LanceDB/Kuzu 后端尚未实现。

对应执行计划见 [auto-publish](docs/superpowers/plans/2026-09-22-auto-publish.md)、[knowledge-graph-core](docs/superpowers/plans/2026-09-22-knowledge-graph-core.md)、[pluggable-storage-engine](docs/superpowers/plans/2026-09-22-pluggable-storage-engine.md)。spec 自述上线前须验证图谱收益、实体合并、蒸馏条件保持等假设；这些尚未验证。
