# 自动发布（Cognee 模式）实施计划

修订：2026-09-23。产品契约见 [整体设计 A1](../specs/2026-09-22-memory-knowledge-platform-design.md)，存储与事务使用同文档 [A2](../specs/2026-09-22-memory-knowledge-platform-design.md#storage-design)。

## 目标、范围与前置条件

去掉候选→审核→发布人工门，使结构化 memory 和 capture 抽取结果直接发布，同时保留 scope、来源、审计、幂等、版本冲突、删除和恢复约束。

本计划在 [本地存储主计划](2026-09-22-pluggable-storage-engine.md) M5 完成后执行。当前候选/审核功能仍存在，本轮文档整合不修改它。禁止照抄旧 PG SQL 或依赖 Apalis/RLS；调用本地版的领域事务和 JobQueue，不重新定义存储接口。

## P1：权限、状态与新库定义

涉及：src/types.rs、SQLite 当前初始化定义、对应结构检查、生命周期测试。

- [ ] memory/capture/knowledge 请求使用 write 权限；自动发布继承合法写入任务并在提交时重新核验身份与权限。
- [ ] publish 是任务操作，不等于 reviewer 权限；restore 等管理动作保留显式授权。
- [ ] 新库不再创建 candidates/reviews/review_id，移除相应约束与结构检查；不修改或升级已有库。
- [ ] publish_if_authorized 作为兼容输入可保留，但不控制是否发布，也不能提升权限。
- [ ] 固定 accepted/publishing/ready/failed 等结果语义；受理成功不代表已可检索。

完成标准：writer 可触发合法自动发布，reader 不可写，已撤销 key 无法提交；新库定义与领域模型一致。

## P2：memory 与 capture 编排

涉及：src/service.rs、src/worker.rs、领域事务接口。

- [ ] memory 在同一 scope 内按 fact_key 定位资产槽，登记 source、expected_version 和 publish job；去掉 candidate/review 中转。
- [ ] 新值通过 expected_version 乐观校验追加版本；冲突保留旧值并给出明确结果，不做无条件覆盖。
- [ ] capture 先保存原始来源，extract 在事务外调用模型，再逐条形成待发布记忆。
- [ ] Prepared 使用独立的多记忆结果类型（如 PublishMemories），保留每条 fact_key/asset/version，不把不同事实拼成一个文档发布。
- [ ] 复用事务内幂等键、作业登记与审计；重复执行不产生重复版本或重复子任务。
- [ ] knowledge 继续走 ingest→publish，统一使用存储主计划的产物账本与发布检查。

完成标准：结构化输入与抽取输入均能自动发布，多事实对应独立资产，重放与冲突行为确定。

## P3：提交、失败与删除

涉及：Worker 提交、JobQueue、artifact_ledger、来源与资产删除。

- [ ] 提交时复核 scope、来源有效性、资产墓碑、generation/run_token、当前权限与 expected_version。
- [ ] 图/向量写入复用账本流程，不以外部写成功直接宣告版本可见。
- [ ] 无模型或 provider/解析错误保留原始来源并返回明确失败，按契约显式重试；数据库错误由 JobQueue 有限重试。
- [ ] 删除先阻断检索与迟到提交，后清理摘要、owner、图和向量；共享来源对象保留。
- [ ] 保留 memory/asset 发布、撤回与恢复审计，不能因为移除 reviews 而失去行为记录。

完成标准：失败可观测、重试不重复发布、撤回不可复活，存储与权限约束不因自动发布削弱。

## P4：API、MCP、测试与文档

涉及：src/api.rs、src/mcp.rs、tests/lifecycle.rs、tests/processes.rs、docs/API.md、README.md、docs/STATUS.md。

- [ ] 移除 /v1/candidates 与 review 路由及对应服务方法；响应不再要求 candidate_id/review_id。
- [ ] 修改 memory/capture 文档和示例为“受理后轮询任务”；job 返回实际发布结果与就绪能力。
- [ ] 覆盖 writer 自动发布、reader 拒绝、key 撤销、同 key 重放、同 fact_key 冲突、多事实抽取、失败重试、取消与强退恢复。
- [ ] 验证 source/asset 删除后 search/resolve/MCP 立即不可见，恢复仍是追加版本。
- [ ] 普通本地测试不依赖 PG；fmt、clippy、单元与真实本地进程套件通过后更新实现状态。

完成标准：用户入口、状态机、初始化结构和文档统一为自动发布，无候选审核残留依赖；不新增数据库升级能力。
