# 云上 Agent 记忆与知识库服务的 Serverless 架构

## 1. 核心结论

一个“知识库”是逻辑概念，不应直接等同于“一套向量索引加一个图数据库”。

更常见的业界架构是：

- 实时库保存权威数据；
- 对象存储保存原文、历史版本和索引文件；
- 向量索引、全文索引和图索引作为派生索引；
- API、检索编排和后台任务按请求量、队列积压和索引负载分别弹性扩缩；
- 小租户共享物理资源，大租户再提升到独立 shard、index 或实例。

因此，实时库与索引通常是一对多关系，而不是一对一关系。

## 2. 推荐的总体架构

```text
Agent / SDK
    ↓
API 层：鉴权、租户路由、配额、限流
    ↓
记忆读写与检索编排层
    ├── 实时状态 / 元数据存储
    ├── 向量检索
    ├── 全文检索
    └── 图检索（按需）

写入事件 / Outbox / CDC
    ↓
持久队列
    ↓
弹性后台任务
    ├── 文档解析、切分
    ├── Embedding
    ├── 事实与实体抽取
    ├── 向量索引更新
    ├── 全文索引更新
    └── 图谱更新

原文、历史版本、索引段
    ↓
对象存储
```

API 和检索编排可以运行在函数或弹性容器上。解析、Embedding、记忆压缩和索引构建更适合由队列驱动的后台任务完成。

## 3. 各类存储的职责

| 数据 | 主要访问模式 | 推荐存储 |
|---|---|---|
| 会话状态、执行 checkpoint | 按 ID 读取、频繁更新、并发控制 | KV 或关系库 |
| 用户偏好、事实型长期记忆 | 精确读取、条件更新、语义召回 | KV/关系库为权威源，向量索引辅助召回 |
| 对话事件、工具调用历史 | 追加写、按会话和时间读取 | 热数据存 KV/日志，冷数据归档对象存储 |
| 原始文档和版本 | 大对象、版本管理 | 对象存储 |
| 文档 chunk、来源、权限 | 元数据过滤、追溯 | 关系库或 KV |
| 向量 | 相似度搜索 | 向量索引 |
| 关键词 | 精确词法搜索、过滤 | 倒排索引 |
| 实体和关系 | 多跳关系、路径查询 | 图数据库，按需启用 |

不需要一开始就部署七套数据库。早期可以使用“对象存储＋一个支持向量和全文检索的数据库＋队列”，在规模和查询模式明确后再拆分。

## 4. 记忆和知识库的写入语义

知识库通常可以异步处理：

```text
上传文档
  → 保存原文和任务
  → 解析、切分、Embedding
  → 构建索引
  → 发布索引版本
```

接口返回 `accepted` 只表示已接收；状态为 `searchable` 后才表示内容可检索。更新文档时先生成新版本，完成后再切换可见版本，避免一次查询混入新旧 chunk。

记忆需要更强的即时性：

- 当前会话状态和明确偏好同步写入权威存储；
- 摘要、事实提取和 Embedding 异步完成；
- 检索时合并已索引记忆和最近尚未索引的增量记录；
- 使用版本号或 CAS 避免旧任务覆盖新事实；
- 使用事务 Outbox 或 CDC，避免权威数据写成功但索引任务没有发布。

## 5. “一个实时库对应一个索引表吗？”

不是。更准确的关系是：

```text
实时库
├── documents
├── chunks
├── memories
├── entities
└── change_log / index_outbox
       ├── 向量索引
       ├── 全文索引
       └── 图索引
```

同一条 chunk 可以同时进入向量索引、全文索引和图索引；不同实时表也可以共同生成同一个查询索引。索引是面向查询方式的物化视图，而不是实时库的简单镜像。

## 6. 多租户的逻辑与物理映射

推荐将租户、知识库和物理资源解耦：

```text
逻辑层：
tenant_id = t1
kb_id     = kb_100

物理层：
共享元数据数据库
共享向量索引集群
共享全文索引集群
共享图数据库（可选）

隔离键：
tenant_id + kb_id
```

常见映射方式：

```text
向量：
  physical_index = embedding_model_v3
  namespace      = tenant_id:kb_id 或 bucket_id
  metadata       = tenant_id, kb_id, document_id, ACL, version

全文：
  共享 search index
  使用 tenant_id、kb_id 过滤或 routing

图：
  共享 graph cluster
  节点和边都带 tenant_id、kb_id
  大租户再拆 database 或实例
```

Pinecone 的生产建议是优先使用 namespace 做数据分隔，而不是为每个租户创建多个 index。Weaviate 的多租户模式则为每个租户使用独立 shard，这说明“逻辑租户”和“物理索引”的映射可以由具体引擎负责，也可以由服务自己的路由层负责。

实时库通常使用共享表加租户键：

```sql
CREATE TABLE chunks (
    tenant_id   uuid NOT NULL,
    kb_id       uuid NOT NULL,
    chunk_id    uuid NOT NULL,
    document_id uuid NOT NULL,
    content     text NOT NULL,
    embedding   vector(1536),
    version     bigint NOT NULL,
    deleted_at  timestamptz,
    PRIMARY KEY (tenant_id, kb_id, chunk_id)
);
```

所有查询都必须带租户条件。关系数据库还可以启用 Row-Level Security，形成数据库层的默认拒绝保护；应用层仍需负责身份认证、授权和租户路由。

## 7. Namespace 数量有限时怎样扩展

namespace 不应和租户一一绑定到物理资源。应增加一层 placement service：

```text
tenant_id + kb_id
        ↓
placement service
        ↓
physical_index / shard / namespace / partition
```

placement 表可以包含：

```text
tenant_id
kb_id
physical_index
partition
namespace
isolation_level
status
version
```

### 7.1 虚拟租户：共享 namespace 加 metadata 过滤

多个小租户共享一个物理 namespace：

```text
physical index: vector-index-07
namespace: bucket-123
filter:
  tenant_id = "t1"
  kb_id = "kb9"
```

namespace 数量只等于 bucket 数量，而不是租户数量。前提是向量引擎支持高效的 metadata pre-filter、partition key 或 tenant-aware routing。若只能在 ANN 召回后过滤，数据量大时可能影响性能和召回准确率。

### 7.2 Namespace 池

```text
bucket-000
bucket-001
...
bucket-999
```

通过 `hash(tenant_id + kb_id) % N` 分配租户。扩容时可以增加 bucket，或者通过一致性哈希减少迁移量。

### 7.3 多物理 index / shard

```text
vector-index-001
vector-index-002
...
vector-index-100
```

placement service 将租户映射到不同物理 index。可以按照哈希、租户大小、地域、合规区域或客户等级分配。

### 7.4 租户分级

```text
小租户：共享 index + bucket + metadata filter
中型租户：独立 namespace 或独立 shard
大型租户：独立 physical index
强合规租户：独立实例或独立账号
```

这比给每个知识库创建独立数据库更适合 Serverless，因为小租户数量多，而每个物理实例都会产生连接、监控、备份、升级和空闲成本。

## 8. 在线迁移和热点拆分

当某个 shard 过热、容量不足或需要提升隔离级别时，采用在线迁移：

```text
旧 shard
   │
   ├── 开启双写
   ├── 从权威库回填新 shard
   ├── 校验数量、版本和查询结果
   ├── placement 切换到新 shard
   └── 删除旧 shard 数据
```

迁移期间可以维护：

```text
placement_version
migration_status
read_index
write_index
```

这样可以先读旧索引、双写新旧索引，完成校验后再切换路由。

## 9. 推荐的默认落地方案

对于大多数 Agent 记忆和企业知识库服务，建议从以下方案开始：

```text
元数据 / 实时状态：
  共享关系库或 KV
  主键带 tenant_id + kb_id

原文：
  对象存储
  路径带 tenant_id / kb_id / document_id

向量：
  共享 vector index
  namespace 使用 bucket 或 tenant_id:kb_id
  metadata 保留租户、知识库、权限和版本

全文：
  共享 search index
  使用 routing 和过滤

图：
  初期按需启用
  共享 cluster，节点和边带租户键
  大租户拆 database 或实例

路由：
  placement service 管理物理资源位置和迁移状态
```

核心原则是：

> namespace 是逻辑隔离和查询路由单位，physical index 或 shard 是容量和性能扩展单位；两者不应绑定成一一对应关系。

另外，向量、全文和图索引都应视为可重建的派生数据。原文、事实记录、事件日志和版本信息才是权威数据。这样在索引损坏、Embedding 模型变化、分片迁移或租户删除时，可以可靠地重建和清理。

