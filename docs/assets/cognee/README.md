# Cognee 分析配图与核验材料

两篇主文档：[架构](../../Cognee_技术架构分析.md)、[技术细节](../../Cognee_技术细节与方案对比.md)。

图展示组件和主要业务流程，不是源码函数调用图。PNG 用于 Markdown/GitHub 直接显示；同名 SVG 可缩放编辑，JSON 保留文字、坐标和连线。图中的逻辑组件不代表独立服务部署。

| 图 | 内容 |
| --- | --- |
| [01_components.png](01_components.png) | 组件、状态与主要交互 |
| [02_knowledge_flow.png](02_knowledge_flow.png) | 资料到可检索知识 |
| [03_recall_flow.png](03_recall_flow.png) | 提问、范围、证据与答案 |
| [04_memory_cycle.png](04_memory_cycle.png) | 会话、改进与后续使用 |
| [05_knowledge_model.png](05_knowledge_model.png) | 原文、摘要、实体、来源 |
| [06_retrieval_lanes.png](06_retrieval_lanes.png) | 两种检索机制 |
| [07_distillation.png](07_distillation.png) | 长期经验提炼与发布 |
| [08_deletion.png](08_deletion.png) | 共享知识删除与恢复 |

## 重建配图

在仓库根目录运行；Python 只用标准库，PNG 渲染使用 Windows System.Drawing 和 Microsoft YaHei UI 字体。

```powershell
python docs/assets/cognee/build_diagrams.py
powershell -NoProfile -ExecutionPolicy Bypass -File docs/assets/cognee/render_diagrams.ps1
```

这里的 ExecutionPolicy 参数仅对本次 PowerShell 进程生效，不修改系统策略。如修改 JSON，直接运行 PowerShell 可以更新 PNG；`build_diagrams.py` 会根据脚本中的定义重新生成全部 JSON/SVG，因此长期修改应同步到该脚本。

## 分析核验

```powershell
python docs/assets/cognee/verify_analysis.py --source .local/research/cognee
```

源目录需检出提交 `663a2dc15d04bc0d7ec2733a2dd604b7ed1b8c8e`。脚本只从源码 AST 提取指定纯函数，不导入完整 Cognee 包、不连接模型或数据库。验证内容为块身份、反馈数值、RRF 排序、图关系评分算例及文档本地链接/固定源码引用。

[source_manifest.json](source_manifest.json) 记录本次引用的源码 SHA-256；[verification_report.json](verification_report.json) 记录核验结果。这些材料不构成集成测试或竞品效果实测。研究用上游检出目录保留在被 Git 忽略的 `.local` 下，不随文档提交。

## 交付复核

2026-09-21 复跑上述校验命令，5 组检查通过，覆盖 42 个引用源码文件和 106 处链接。已逐张查看 8 张 PNG，中文文字、箭头和注释清晰，未发现文字截断或相互遮挡。上游检出目录没有未提交改动。

自动校验覆盖数值算例、引用路径与代码围栏；配图可读性通过目视复核。该结果不表示已逐条自动验证正文结论，也不覆盖模型与数据库集成、吞吐延迟或竞品胜负。
