# 评估起点

`cases.jsonl` 是冻结样例格式，不是已完成的竞品 benchmark。生命周期与安全门槛由 `tests/lifecycle.rs`、`tests/processes.rs` 可执行验证；检索样例作为后续真实冻结数据集的起点，不把模拟 embedding 的排序算作真实语义质量。

扩充评估时分别记录：检索 Recall@k/来源正确率，候选泄露率/跨 workspace 泄露率，版本冲突和删除不复活，实际 Agent 引用与任务完成率，p50/p95/p99 和模型成本。正确性门槛不应被平均相关性抵消。

尚未运行 Cognee 或其他 ContextDB 产品对比；“rds contextdb”具体产品身份仍待确认。性能和胜负结论必须附硬件、数据集版本、模型配置、运行命令和原始结果。
