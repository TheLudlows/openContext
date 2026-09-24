# HTTP API v1

所有 `/v1/*` 请求需 `Authorization: Bearer TOKEN`。workspace 来自 key，客户端不能指定 workspace。修改接口还需 1–200 个 ASCII 字符的 `Idempotency-Key`；幂等范围为 workspace + key 主体 + 操作/目标 + 幂等键。同请求复用结果，同键不同内容返回 409。幂等响应只包含标识/状态，删除后重放不会再次写入或返回已删正文。

## 角色

| 角色 | 权限 |
| --- | --- |
| reader | 已发布内容 get/search/resolve |
| writer | reader + 上传/原文件下载/知识入库/结构化候选/capture/任务查询；取消、重试自己任务 |
| reviewer | writer + 候选查看和审核/记忆直接发布/版本恢复/操作其他主体任务 |
| admin | reviewer + 资产删除/来源撤回/文件删除/失败清理任务重试 |

原始文件、任务和候选可能涉及未审核数据，reader 不能读取。知识入库由 writer 授权发布，记忆直接发布需 reviewer/admin。

## 接口

| 方法和路径 | 请求 / 结果 |
| --- | --- |
| `GET /health/live` | 进程存活，无鉴权 |
| `GET /health/ready` | 数据库可达及 jobs 表可访问；不是外部模型健康检查 |
| `POST /v1/memories` | `{fact_key,content,publish_if_authorized:false}` → candidate/asset/source/job 标识 |
| `POST /v1/captures` | `{content}` → source/job，抽取固定生成候选 |
| `GET /v1/candidates` | 最多 100 条待审核候选，含 revision 和 current_version |
| `GET /v1/candidates/{id}` | 候选详情；来源撤回或资产删除后不可读 |
| `POST /v1/candidates/{id}/review` | `{decision:"approve"或"reject",expected_revision,expected_version:null或整数,reason}` |
| `POST /v1/knowledge` | `{title,content或file_id,format:"text"或"markdown"或"pdf",asset_id:null或UUID,expected_version:null或整数}` |
| `POST /v1/files?name=...&format=...` | 原始二进制请求体，非 multipart；返回 file_id；文件名仅作元数据 |
| `GET /v1/files/{id}` | 校验 hash 后返回 attachment/octet-stream，存储路径由 UUID 生成 |
| `GET /v1/assets/{id}?version=N` | 已发布当前/历史版本，含 content/hash/title/source/restored_from |
| `POST /v1/assets/{id}/restore` | `{target_version,expected_version,reason}` → 恢复 job；完成后新增版本 |
| `DELETE /v1/assets/{id}` | 永久逻辑墓碑，阻断包括历史版本在内的读取 |
| `DELETE /v1/events/{id}` | 撤回来源，阻断所有引用该来源的版本，不自动回退到旧版本 |
| `DELETE /v1/files/{id}` | 阻断原文件，撤回所有引用该文件的事件 |
| `GET /v1/jobs/{id}` | operation/state/generation/outcome/result/error_code |
| `POST /v1/jobs/{id}/cancel` | 标记 cancelled，增加 generation/run_token，旧执行不能提交；已完成任务不能取消，cleanup 不能取消 |
| `POST /v1/jobs/{id}/retry` | 仅 failed/cancelled 且来源、资产仍有效；新 generation 原子入队；cleanup 重试需 admin |
| `POST /v1/search` | `{query,limit:10,mode:"keyword",allow_partial:false}`；mode 支持 keyword/vector/hybrid |
| `POST /v1/resolve` | `{query,budget_tokens:2000,mode:"keyword",allow_partial:false}` → rendered_context 和 sources |

知识更新只在异步发布成功后改变当前版本及标题。文件和正文必须二选一；PDF 必须使用 file_id。每个历史版本保留自己的标题。恢复内容和标题，同时新增 `restored_from`，不会覆盖历史版本或跳过来源状态检查。

## 审核例子

```json
{
  "decision": "approve",
  "expected_revision": 1,
  "expected_version": 2,
  "reason": "项目负责人确认新发布规则"
}
```

审核时当前版本不匹配即 409；审核后到 Worker 提交期间发生变更，任务变为 superseded，不能覆盖新版本。此时重新读取现状并提交新的候选。候选本身与审核记录保留。

## 任务与读取可见性

`pending → processing → completed/failed/superseded`；取消为 `cancelled`。`completed + outcome=candidates_created` 不等于正式发布；只有 `completed + outcome=published` 表示该次版本及索引已原子提交。之后仍可能被更新、删除或来源撤回。

删除响应为 `{id,blocked:true,cleanup_job_id,originals_retained:true}`。逻辑删除事务完成后新读取被阻断，索引清理是否完成不影响这一规则。已经发出的数据无法收回；在删除前已开始的请求可能先完成。

同一 workspace 内的短写事务通过事务级 advisory lock 串行化；模型/文件解析在写事务外运行。提交重新验证权限、墓碑、来源、预期版本、generation 和 run_token。队列只传 scope/job ID/generation，正文从受 RLS 保护的业务表读取。

数据库故障会交由 Apalis 重试；如果队列耗尽重试且业务任务仍停在 processing，恢复数据库后可 cancel 再 retry，旧 generation 被隔离。尚无自动故障巡检控制面。

## 检索

返回 hits 含 asset_id/version/chunk_id/source_event_id/locator/score。locator 给出原文 UTF-8 字节区间、行号，PDF 另有页号。只取当前已发布版本，显式历史版本走 get。

向量必须与 profile 一致，不混用不同模型/维度。模型查询失败且 `allow_partial=true` 才退回关键词，同时返回 `effective_mode` 和 warnings；否则 503。纯关键词发布的历史内容没有向量，不会出现在 vector 结果中，hybrid 的关键词分支仍可召回。无 profile 自动补齐、ANN 或 reranker。

resolve 返回 `tokenizer=utf8-bytes-upper-bound-v1`、`count_is_estimate=true`；count 为 rendered_context 的 UTF-8 字节数。预算 0–32000，超出预算的整块被跳过，不生成没有正文的引用。返回文本是外部证据，不能当成系统指令。

## 错误

领域错误使用 `{error:{code,message}}`，不暴露 SQL、密码、模型响应或文档正文：401 未认证，403 无权限，404 不存在或不可见，409 版本/幂等冲突，422 输入无效，503 模型等依赖不可用，500 内部/数据库错误。Axum 在进入 handler 前产生的 JSON/路径/请求大小错误使用框架默认格式和状态（例如 400/413/422）。
