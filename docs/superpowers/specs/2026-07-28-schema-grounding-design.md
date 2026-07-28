# Schema Grounding — 向量检索增强 Design Spec

> **Date:** 2026-07-28
> **Status:** Draft

## Goal

为 VlorQl 的 Schema 选择机制引入向量检索能力，替代当前的纯 TF-IDF 方案，在大型数据库场景下提升表/列检索的语义准确性。通过配置开关控制，默认不使用向量检索（保持零额外依赖）。

## Architecture

```
用户问题
    │
    ▼
[PromptBuilder.filter_relevant_tables()]
    │
    ├─ [向量检索启用] → 问题 Embedding → Qdrant 检索 top-K
    │                       │
    │                       ▼
    │                  [结果合并: 向量权重 + TF-IDF 权重]
    │
    └─ [向量检索禁用] → [现有 TF-IDF 逻辑]（默认路径）
                            │
                            ▼
                      [FK 邻居扩展] → 返回相关表列表
```

## Tech Stack

- **向量数据库:** Qdrant（Apache-2.0, Docker 部署, REST/gRPC API）
- **Embedding:** `all-MiniLM-L6-v2`（384维, ONNX 运行时）或 OpenAI Embedding API
- **Rust 客户端:** `qdrant-client` crate（Apache-2.0）
- **配置:** `LlmConfig` 或 `DialectProfile` 中新增 `vector_search` 字段

## File Structure

| 文件 | 责任 |
|------|------|
| `crates/vlorql-core/src/prompt/schema_index.rs`（新建） | Schema → 文本块 → Embedding → Qdrant 索引 |
| `crates/vlorql-core/src/prompt/builder.rs`（修改） | `filter_relevant_tables()` 集成向量检索开关 |
| `crates/vlorql-core/src/prompt/mod.rs`（修改） | 导出 `SchemaIndexer` |
| `Cargo.toml`（修改） | 可选依赖 `qdrant-client` |

## Design

### 1. 配置开关

在 v0.5.0 中已有 `LlmConfig` 结构（`vlorql-llm/src/lib.rs`），可以在其中添加 `vector_search` 字段，或更合理地在 `SchemaSnapshot` 或 `DialectProfile` 中添加。

**推荐位置:** 在 `PromptBuilder` 中新增 `vector_search: bool` 字段（默认 `false`），用户通过 `VlorQlBuilder` 暴露 `with_vector_search(bool)` 方法控制。

```rust
// PromptBuilder 新增字段
pub struct PromptBuilder {
    pub schema: Arc<SchemaSnapshot>,
    pub dialect: DialectProfile,
    pub policy: PolicyConfig,
    pub skill: Option<Arc<PromptSkill>>,
    pub include_examples: bool,
    pub vector_search: bool,  // ← 新增
}
```

### 2. SchemaIndexer 组件

在首次使用向量检索时（懒加载），对所有表/列的文本描述计算 Embedding 并存入 Qdrant。

```rust
/// Indexes schema tables/columns into a vector store for semantic retrieval.
pub struct SchemaIndexer {
    client: qdrant_client::Qdrant,
    collection_name: String,
}

impl SchemaIndexer {
    /// Connect to Qdrant server.
    pub async fn connect(url: &str) -> Result<Self, VlorQLError>;

    /// Build/rebuild the index from a SchemaSnapshot.
    /// Each table and column becomes a separate point with its text description.
    pub async fn index_schema(&self, schema: &SchemaSnapshot) -> Result<(), VlorQLError>;

    /// Search for tables/columns relevant to the user question.
    pub async fn search(&self, question: &str, top_k: u64) -> Result<Vec<String>, VlorQLError>;
}
```

### 3. Schema 文本化格式

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

### 4. filter_relevant_tables 集成

```rust
pub fn filter_relevant_tables(&self, user_question: &str) -> Vec<String> {
    if self.schema.tables.is_empty() {
        return Vec::new();
    }

    // 向量检索路径
    if self.vector_search {
        if let Some(indexer) = &self.schema_indexer {
            match indexer.search(user_question, 5).await {
                Ok(tables) if !tables.is_empty() => {
                    let expanded = self.expand_foreign_key_neighbors(&tables.into_iter().collect());
                    return self.schema.tables.iter()
                        .filter(|t| expanded.contains(&t.name))
                        .map(|t| t.name.clone())
                        .collect();
                }
                _ => {} // 回退到 TF-IDF
            }
        }
    }

    // 原有 TF-IDF 路径（也是回退路径）
    // ... 现有代码不变 ...
}
```

### 5. 依赖管理

在 `Cargo.toml` 中使用 feature gate：

```toml
[features]
default = []
vector-search = ["qdrant-client"]
```

```toml
[dependencies]
qdrant-client = { version = "1.7", optional = true }
```

## Global Constraints

- 向量检索**默认关闭**，用户通过 `VlorQlBuilder::with_vector_search(true)` 显式开启
- 不增加现有用户（不使用向量检索）的依赖和编译时间
- Qdrant 需要用户自行部署（Docker: `docker run -p 6333:6333 qdrant/qdrant`）
- Embedding 生成通过首次使用的懒加载初始化，不阻塞启动
- 向量检索失败时静默回退到 TF-IDF
