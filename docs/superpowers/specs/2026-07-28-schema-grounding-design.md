# Schema Grounding — 向量检索增强 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

为 VlorQl 的 Schema 选择机制引入向量检索能力，替代当前的纯 TF-IDF 方案，在大型数据库场景下提升表/列检索的语义准确性。通过配置开关控制，默认不使用向量检索（保持零额外依赖）。

## 核心数据流

```
                                                         ┌─────────────────────┐
                                                         │     数据库 (DB)      │
                                                         │  (PostgreSQL/MySQL/  │
                                                         │   SQLite/etc)        │
                                                         └──────┬──────────────┘
                                                                │ 读取表/列/类型/FK
                                                                ▼
┌──────────────────────────────────────────────────────────────────────────────────┐
│                         SchemaSnapshot (schema/snapshot.rs)                      │
│   Vec<TableSchema> ──→ name: String, columns: Vec<ColumnSchema>, description    │
│                          data_type, is_primary_key, foreign_key                  │
└──────────────────────────────────────────────────────────────────────────────────┘
          │                                              │
          │ ① 用户调用                                    │ ② 向量检索启用时
          │ VlorQlBuilder                                 │
          │ .with_schema(snapshot)                        ▼
          │                                           [SchemaIndexer 懒加载]
          ▼                                               │
 ┌────────────────┐                              ┌────────┴───────────┐
 │  用户问题       │                              │ 表/列 → 文本化      │
 │  "查询客户..."  │                              │ "users: id int,    │
 └───────┬────────┘                              │  name string"      │
         │                                       └────────┬───────────┘
         ▼                                                │ Embedding 模型
 ┌────────────────┐                              ┌────────┴───────────┐
 │filter_relevant │                              │ 向量数据库 (Qdrant)  │
 │_tables()       │                              │ 存储: 文本→向量映射  │
 └───────┬────────┘                              └────────┬───────────┘
         │                                                │
         ├── [向量检索关闭] → TF-IDF 语义匹配 ─────────────┘
         │
         └── [向量检索开启] → 问题 Embedding → Qdrant 检索 top-K
                                           │
                                           ▼
                                     [TF-IDF + 向量结果合并]
                                           │
                                           ▼
                                    [FK 邻居扩展]
                                           │
                                           ▼
                                     [筛选后的 Schema → Prompt]
```

**关键区分：**
- Schema **本身**来自数据库连接（DDL 读取），不是 embedding 生成的
- Embedding 只用于**检索**：对 Schema 的文本描述（"users: id int, name string"）计算一次向量，存入 Qdrant
- 用户提问时，对问题也计算向量，在 Qdrant 中找到**语义最相似**的表/列描述，从而推断应该把哪些表放入 prompt

## Tech Stack

- **向量数据库:** Qdrant（Apache-2.0, Docker 部署, REST/gRPC API）
- **Embedding:** `all-MiniLM-L6-v2`（384维, ONNX 运行时）或 OpenAI Embedding API
- **Rust 客户端:** `qdrant-client` crate（Apache-2.0）
- **配置开关:** `PromptBuilder.vector_search: bool`（默认 `false`）

## File Structure

| 文件 | 责任 |
|------|------|
| `crates/vlorql-core/src/prompt/schema_index.rs`（新建） | Schema 文本化 → Embedding → Qdrant 索引/检索 |
| `crates/vlorql-core/src/prompt/builder.rs`（修改） | `filter_relevant_tables()` 集成向量检索开关 |
| `crates/vlorql-core/src/prompt/mod.rs`（修改） | 导出 `SchemaIndexer` |
| `Cargo.toml`（修改） | 可选依赖 `qdrant-client` |

## Design

### 1. 配置开关

在 `PromptBuilder` 中新增 `vector_search: bool` 字段（默认 `false`），用户通过 `VlorQlBuilder` 暴露 `with_vector_search(bool)` 方法控制。

```rust
// PromptBuilder 新增字段
pub struct PromptBuilder {
    pub schema: Arc<SchemaSnapshot>,
    pub dialect: DialectProfile,
    pub policy: PolicyConfig,
    pub skill: Option<Arc<PromptSkill>>,
    pub include_examples: bool,
    pub vector_search: bool,        // ← 新增: 默认 false
    pub schema_indexer: Option<Arc<SchemaIndexer>>,  // ← 新增: 懒加载
}
```

### 2. SchemaIndexer 组件

负责：接收已有 Schema → 文本化 → Embedding → 写入 Qdrant。首次使用时懒加载初始化。

```rust
/// Indexes schema table/column text descriptions into Qdrant for
/// semantic retrieval. The Schema itself comes from the database
/// (SchemaSnapshot), NOT from embedding.
pub struct SchemaIndexer {
    client: qdrant_client::Qdrant,
    collection_name: String,
}

impl SchemaIndexer {
    /// Connect to Qdrant server (user must deploy Qdrant separately).
    pub async fn connect(url: &str) -> Result<Self, VlorQLError>;

    /// Build/rebuild the vector index from an existing SchemaSnapshot.
    /// Each table and column becomes a separate point in Qdrant, storing
    /// its text description as a vector for similarity search.
    pub async fn index_schema(&self, schema: &SchemaSnapshot) -> Result<(), VlorQLError>;

    /// Search for tables semantically relevant to the user question.
    /// Returns table names sorted by relevance, using vector similarity.
    pub async fn search(&self, question: &str, top_k: u64) -> Result<Vec<String>, VlorQLError>;
}
```

### 3. Schema 文本化格式（用于向量检索）

生成文本片段是为了**计算搜索向量**，而非用来替换 Schema 数据。实际 Schema 数据（表名、列名、类型）始终来自 `SchemaSnapshot`。

每个表生成一段 Embedding 文本：

```
Table: users — user accounts
Columns: id int, name string, email string, created_at timestamp
FK: orders.user_id → users.id
```

每个列独立生成一段 Embedding 文本：

```
Column: users.name string — user's display name
```

**当用户问 "查询客户信息" 时：**
1. 问题向量化 → Qdrant 搜索 → 返回 `["users", "users.name", "users.email"]`
2. 提取表名 `["users"]` → FK 邻居扩展 → 可能包含 `["users", "orders"]`
3. 用这些表名从 `SchemaSnapshot` 中**取出完整的表结构** → 注入 prompt

### 4. filter_relevant_tables 集成

```rust
pub fn filter_relevant_tables(&self, user_question: &str) -> Vec<String> {
    if self.schema.tables.is_empty() {
        return Vec::new();
    }

    // 向量检索路径（开启时优先）
    if self.vector_search {
        if let Some(indexer) = &self.schema_indexer {
            match indexer.search(user_question, 5) {
                Ok(tables) if !tables.is_empty() => {
                    let set: HashSet<String> = tables.into_iter().collect();
                    let expanded = self.expand_foreign_key_neighbors(&set);
                    return self.schema.tables.iter()
                        .filter(|t| expanded.contains(&t.name))
                        .map(|t| t.name.clone())
                        .collect();
                }
                _ => { /* 回退到 TF-IDF */ }
            }
        }
    }

    // 原有 TF-IDF 路径（默认，也是向量检索的 fallback）
    // ... 现有的 filter_relevant_tables 代码不变 ...
}
```

### 5. 依赖管理

使用 feature gate 控制，不增加默认用户的依赖：

```toml
[features]
default = []
vector-search = ["qdrant-client"]
```

```toml
[dependencies]
qdrant-client = { version = "1.7", optional = true }
```

用户启用方式：

```rust
// Cargo.toml 中添加 feature
vlorql-core = { path = "../vlorql-core", features = ["vector-search"] }

// 代码中开启
VlorQl::builder()
    .with_schema(schema)
    .with_vector_search(true)
    .build()?;
```

## 总结：Schema 数据流 vs Embedding 数据流

| 数据 | 来源 | 用途 |
|------|------|------|
| `SchemaSnapshot`（表名/列名/类型/FK） | 数据库 DDL | 注入 prompt 给 LLM 看的实际 Schema |
| 文本描述（"users: id int"） | 从 SchemaSnapshot 生成 | 计算 Embedding 向量，用于检索 |
| Embedding 向量 | 对文本描述计算得到 | 存入 Qdrant，语义匹配问题 |
| 用户问题向量 | 运行时对问题计算 | 在 Qdrant 中搜索相似表/列 |

## Global Constraints

- 向量检索**默认关闭**，用户通过 `VlorQlBuilder::with_vector_search(true)` 显式开启
- 不增加现有用户（不使用向量检索）的依赖和编译时间
- Qdrant 需要用户自行部署（Docker: `docker run -p 6333:6333 qdrant/qdrant`）
- Embedding 计算通过首次使用的懒加载初始化，不阻塞启动
- 向量检索失败时静默回退到 TF-IDF
