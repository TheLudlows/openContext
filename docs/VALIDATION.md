# 验证记录

## 2026-09-23：M0 本地 Rust 库探针（Windows 通过）

隔离工程位于 [tools/storage-probe](../tools/storage-probe/README.md)，直接依赖 SQLx 0.8.6、LanceDB 0.23.1（Lance 1.0.1、Arrow 56.2.1）、Kuzu 0.11.3。Windows x64/MSVC、Rust 1.88.0 完整可执行文件构建和 **14 项真实文件/子进程检查通过**。主应用尚未接入这些库；探针通过不代表生产后端已经交付。

| 后端 | 检查数 | 实际通过范围 |
| --- | --- | --- |
| SQLite | 4 | 重复初始化、跨 tenant/workspace 同 ID（含引号）、删除后重开；双进程写入与长连接刷新；已确认写入后强退；WAL 下未提交写不可见、写锁竞争、强退回滚及后续写入 |
| LanceDB | 3 | 相同 scope/重开检查及带 scope 预过滤的精确向量 top-1；双进程追加与长连接刷新；已确认写入后强退 |
| Kuzu | 7 | 相同 scope/重开检查；已确认写入后强退；带 scope 的边及 DETACH DELETE；读写宿主文件锁与单宿主命令访问；双只读进程；未提交事务强退回滚；共享 Database 的跨线程连接在写事务前后读取正确快照 |

关键输出：SQLite 竞争写入报 `database is locked`；Kuzu 写宿主存在时，第二个读写或只读进程均报 `Could not set lock on file`。单宿主多连接通过，因此本地目标调整为 API/Worker 同进程共享引擎。LanceDB 使用 `read_consistency_interval(Duration::ZERO)`，持久连接能看到其他进程的提交。

实际命令：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tools/storage-probe/build.ps1 -Toolchain 1.88.0 -TargetDirectory target/storage-probe-msrv -Jobs 16
python tools/storage-probe/verify.py --binary target/storage-probe-msrv/debug/opencontext-storage-probe.exe
```

完整报告：[report.json](../target/storage-probe/runs/run-69exqwqr/report.json)。报告和临时数据库留在本地、不纳入 Git，脚本可重建结果。之前也在 Rust 1.96.0 以 `--no-default-features --features sqlite,vector` 构建并通过 SQLite 4 项、LanceDB 3 项检查；不将其描述为三个库在 1.96.0 上的完整构建通过。

本轮固定的构建条件：

- MSVC C++ 工具链、CMake、Ninja；Lance 使用构建辅助 crate 提供的 protoc。这些是构建工具，测试没有启动数据库服务。
- Kuzu 固定 `cxx = 1.0.138`，但默认解析到的 `cxx-build 1.0.202` 生成不兼容符号，最终链接出现 117 个未解析符号。探针显式固定生成器为 1.0.138。
- Kuzu 保留默认扩展特性。关闭默认特性时，原生构建仍含扩展加载器，Rust 未链接扩展，出现 algo/fts/json/vector 的 4 个未解析符号；恢复默认特性后最终链接和运行通过。LanceDB 关闭默认云端特性。

未覆盖：Linux/macOS、release、预编译 Kuzu 外部库分发、并发初始化和完整结构兼容检查、生产权限/来源校验、完整 API/Worker 及 CLI/MCP 转发、Worker OS 锁与队列、跨库账本、ANN、长期并发和断电恢复。LanceDB 探针写入为唯一测试 ID 的追加，尚非业务幂等 upsert；强杀进程不等于断电测试。后续按 M1–M5 接入和验收。

## 2026-09-23：自动初始化与存储设计调整

- `cargo fmt --all -- --check`、`cargo clippy --locked --offline --all-targets -- -D warnings`、`cargo test --locked --offline` 通过；7 个单元测试通过。
- CLI `--help` 已确认移除旧数据库升级命令。
- 新增 `tests/initialization.rs`：在独立临时数据库验证连接时自动初始化、并发启动、重复初始化保留数据、队列入队函数、不兼容结构拒绝且数据保留、不创建升级历史表。CI 已配置运行此测试。
- initialization、lifecycle、processes 三个数据库集成套件本次均未执行：本机未配置测试数据库环境变量，Docker daemon 未运行。因此尚未验证本次 SQL 初始化定义在真实 PostgreSQL 上的执行结果，也未运行容器构建。
- 此次自动初始化改动未接入 SQLite/LanceDB/Kuzu；独立 M0 探针的后续运行记录见上节。

## 历史基线

以下为 2026-09-21 历史基线，不能作为本次自动初始化实现的验收记录。

日期：2026-09-21。Windows 本机 Rust 1.96.0，独立 Docker PostgreSQL 17 + pgvector，数据库管理员与 `oc_runtime` 分离。容器构建使用 Rust 1.96 / Debian bookworm。

| 检查 | 实际结果 |
| --- | --- |
| `cargo check --locked --offline` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets --locked --offline -- -D warnings` | 通过 |
| `cargo test --locked --offline` | 3 个单元测试通过；2 个需要数据库的测试明确 ignored |
| `cargo test --locked --offline --test lifecycle -- --ignored` | 实际数据库运行通过，1 个综合套件，约 2.5 秒 |
| `cargo test --locked --offline --test processes -- --ignored` | 实际子进程运行通过，1 个综合套件，约 16.7 秒 |
| `docker compose config --quiet` | 通过 |
| `docker build -t opencontext:local .` | Linux release 镜像构建成功 |
| 独立 `opencontext-smoke` Compose 项目 | 数据库初始化/运行角色、workspace 创建、HTTP ready、Worker 发布、get 正文、Files 卷上传均通过；完成后停止服务 |

以上时间只是单次测试耗时，不是服务性能指标。CI 工作流已加入仓库；本记录不代表远端 CI 已运行或通过。

## 生命周期套件覆盖

RLS 未设置 scope 时默认拒绝、跨 workspace 写入失败、跨 workspace get/search/candidate/job/file 无泄露；writer 不能直接确认记忆；reader 无候选/原文件权限；同键相同请求与并发复用、不同请求冲突；重复执行不增加版本；过期审核拒绝；冲突旧值保留；恢复追加 v3；来源撤回阻断相关历史版本；墓碑阻断排队发布及重试；取消与 generation；文件撤回；无模型 capture 明确失败；混合检索显式降级；业务与 Apalis 共同回滚；长 UTF-8 文本及空白区间保真；历史标题和恢复标题；角色降级不保留旧权限；HTTP 认证和 key 撤销。

## 进程套件覆盖

真实 HTTP API + 独立 Apalis Worker；本地确定性模型 stub 的 embedding、hybrid、extract；外部模型自报发布标记不能越权；PDF CLI 子进程解析及页码引用；无效 PDF 失败；第二个 Worker 被拒绝；处理中杀死 Worker 后真实队列恢复、run_token 增加且只发布一个版本；处理中删除不复活；MCP initialize/tools/list/tools/call；同一 MCP 进程下一次调用拒绝已撤销 key。

## 未测试或不包含

真实付费模型供应商的语义质量/费用；主应用的 Rust 1.88 最低版本（独立存储探针已通过）；CI 云端执行；生产 SSO、物理擦除和备份恢复；长期并发/多租户公平性；多 Worker 扩容；向量 generation 在线切换；OS 沙箱隔离；压力/容量/竞品评测。完整后续清单见 [STATUS](STATUS.md)。
