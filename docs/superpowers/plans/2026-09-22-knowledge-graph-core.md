# 知识图谱与证据检索实施计划

修订：2026-09-23。设计依据为 [整体设计第 5、6、9 节及 A2](../specs/2026-09-22-memory-knowledge-platform-design.md#storage-design)。本计划细化 [本地存储主计划](2026-09-22-pluggable-storage-engine.md) 的 M4，不另建 PG 存储层。

## 目标、基线与依赖

目标是本地 SQLite/LanceDB/Kuzu 上的实体/关系抽取、摘要召回、图证据、溯源和可靠删除。当前 graph.rs 的纯逻辑和 PG 实现可作为行为参考；现有写入计数测试不能证明来源撤回、资产删除与共享 owner 正确。

前置条件：主计划 M0 确认访问方式、M1 定义接口、M2 提供 SQLite 事务与 scope、M3 提供账本和对账。可提前开展纯逻辑测试；正式图写入与检索接入必须通过上述前置门槛。

## G1：抽取与确定性身份

涉及：src/types.rs、src/graph.rs、src/models.rs 及纯逻辑测试。

- [ ] 保留 GraphExtraction 显式中间结果；模型仅产生数据，不直接写存储。
- [ ] 校验 entities/relations 数量、空名称、长度、非法字符和关系端点；不执行输入中的指令。
- [ ] 实体身份基于规范化名称、关系身份基于端点和谓词；存储 key 同时包含 scope，跨 workspace 同名对象不合并。
- [ ] 固定规范化与重放规则，覆盖大小写、空白、同名、重复关系和未知端点。

完成标准：同输入重放得到稳定身份；不合法抽取不进入后端。

## G2：图写入与摘要

涉及：src/worker.rs、src/models.rs、src/storage/kuzu.rs、src/storage/ledger.rs、SQLite 初始化定义。

- [ ] ingest 解析 chunk 后生成可选摘要和 GraphExtraction，模型/文件 IO 不持关系事务。
- [ ] SQLite 保存摘要、有效 source/version、owner 和账本；Kuzu 保存实体/关系及查询所需投影。
- [ ] 使用 GraphStore 与 M3 写入流程，禁止 worker/领域服务直接执行 PG SQL 或 Kuzu 查询。
- [ ] 摘要与图抽取的可选失败按实际能力返回状态/warnings；存储失败不得伪装成图已完成。
- [ ] 当前短事实发布和历史恢复是否生成图按明确的能力规则处理，不默认为所有入口都已有图。

完成标准：重复写不重复产生图对象或 owner，部分写失败可重放，发布结果准确表达就绪能力。

## G3：检索与上下文

涉及：src/retrieval.rs、src/graph.rs、GraphStore/VectorStore 接口、HTTP/MCP 测试。

- [ ] 摘要命中映射回有效 chunk，作为独立有序列表参与 RRF；关系扩展保留原文出处。
- [ ] 实体检索及一跳邻域必须限定 scope；返回前通过 SQLite 再确认 owner/source/version/asset 可见性。
- [ ] source 已撤回、资产已删除、非有效版本的证据不得出现在 search 或 resolve 中，即使后台尚未清理。
- [ ] 图证据带对象 ID、来源事件/版本和可解析引用，不能只给无法回溯原文的实体名。
- [ ] 统一预算计算，包括引用标记与 UTF-8 文本，片段整段保留或舍弃。
- [ ] 能力缺失遵循 A2.7 的 allow_partial/effective_mode/warnings 契约；身份与有效性不可降级。

完成标准：原文、摘要和图结果都可溯源，HTTP 与 MCP 一致，预算内无跨 scope 或失效证据。

## G4：删除与共享来源

涉及：领域删除规划、SQLite owners/ledger、GraphStore/VectorStore 删除、恢复测试。

- [ ] 删除/撤回事务先标记逻辑不可见并阻断迟到提交，再登记清理任务。
- [ ] 先删除依赖 chunk 的摘要；图侧先移除被撤回来源的 owner，再按有效 owner 判断并清理孤立关系/实体。
- [ ] 保留仍有其他有效来源支撑的共享对象，不能仅按实体 ID 全局删除。
- [ ] 部分清理失败进入账本重试；强退重启后继续，不恢复已撤回证据。

完成标准：两个来源共享实体时，撤回一个仍保留，撤回最后一个后不可检索且最终清理；带摘要文档删除不被外键阻断。

## 验收用例

- [ ] 相同 ID/名称跨 tenant 与 workspace 隔离。
- [ ] 实体/关系/摘要真实写入、重复执行和部分失败重放。
- [ ] 停止清理 Worker 后撤回来源，图与向量仍立即不可见。
- [ ] 已发布文档生成摘要后删除资产；共享 owner 保留与最终孤儿清理。
- [ ] 处理中取消或删除后迟到结果不能重新发布。
- [ ] search/resolve/MCP 的图引用可溯源且总预算正确。

主计划 M4 只有在真实 Kuzu/LanceDB/SQLite 集成验证通过后才能完成；纯逻辑单测和 PG 基线结果只作辅助证据。
