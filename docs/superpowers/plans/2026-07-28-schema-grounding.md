# Schema Grounding — 向量检索增强 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 VlorQl 的 Schema 选择机制引入 Qdrant 向量检索作为可选的语义检索增强，默认关闭。

**Architecture:** 在 `vlorql-core` crate 中新建 `prompt/schema_index.rs` 模块，对接 Qdrant 向量数据库。通过 `PromptBuilder.vector_search: bool` 开关控制，默认 false。使用 feature gate 控制 `qdrant-client` 依赖，不增加默认用户的编译负担。

**Tech Stack:** Rust, `qdrant-client` (optional), `reqwest`, `serde_json`, Qdrant 向量数据库

## Global Constraints

- 向量检索默认关闭，用户通过 `VlorQlBuilder::with_vector_search(true)` 显式开启
- 不增加现有用户（不使用向量检索）的依赖和编译时间
- Qdrant 需要用户自行部署（Docker）
- 向量检索失败时静默回退到 TF-IDF

---

### Task 1: SchemaIndexer 模块骨架 + Cargo.toml feature gate

**Files:**
- Create: `crates/vlorql-core/src/prompt/schema_index.rs`
- Modify: `crates/vlorql-core/src/prompt/mod.rs`
- Modify: `crates/vlorql-core/Cargo.toml`

**Interfaces:**
- Produces: `pub struct SchemaIndexer { client, collection_name }`
- Produces: `impl SchemaIndexer { connect(), index_schema(), search(), health_check() }`

- [ ] **Step 1: 在 `Cargo.toml` 中添加 feature gate 和可选依赖**

```toml
[features]
default = []
vector-search = ["qdrant-client"]

[dependencies]
qdrant-client = { version = "1.13", optional = true }
```

- [ ] **Step 2: 创建 `schema_index.rs`**

```rust
//! Vector-based schema retrieval using Qdrant.
//!
//! The SchemaSnapshot itself comes from the database (DDL). This module
//! takes the existing SchemaSnapshot, generates text descriptions for
//! each table/column, embeds them into Qdrant, and provides semantic
//! search to find relevant tables for a user question.

use crate::schema::SchemaSnapshot;
use serde_json::Value;

/// Indexes schema table/column text descriptions into Qdrant for
/// semantic retrieval. Lazy-initialized on first use.
pub struct SchemaIndexer {
    client: qdrant_client::Qdrant,
    collection_name: String,
}

#[cfg(feature = "vector-search")]
impl SchemaIndexer {
    /// Connect to a running Qdrant instance.
    pub async fn connect(url: &str) -> Result<Self, crate::errors::VlorQLError> {
        let client = qdrant_client::Qdrant::from_url(url)
            .build()
            .map_err(|e| crate::errors::VlorQLError::config(
                crate::errors::ConfigErrorKind::ConfigFileError {
                    path: "vector_search".into(),
                    reason: format!("Failed to connect to Qdrant: {e}"),
                },
                serde_json::json!({"url": url}),
            ))?;
        Ok(Self {
            client,
            collection_name: "vlorql_schema".to_owned(),
        })
    }

    /// Build/rebuild the vector index from an existing SchemaSnapshot.
    /// Each table becomes a point; each column becomes a point.
    pub async fn index_schema(&self, schema: &SchemaSnapshot) -> Result<(), crate::errors::VlorQLError> {
        // TODO: implement in Task 2
        Ok(())
    }

    /// Search for tables semantically relevant to the user question.
    /// Returns table names sorted by relevance.
    pub async fn search(&self, question: &str, top_k: u64) -> Result<Vec<String>, crate::errors::VlorQLError> {
        // TODO: implement in Task 2
        Ok(vec![])
    }

    /// Check Qdrant connection health.
    pub async fn health_check(&self) -> Result<(), crate::errors::VlorQLError> {
        self.client.health_check().await
            .map_err(|e| crate::errors::VlorQLError::config(
                crate::errors::ConfigErrorKind::ConfigFileError {
                    path: "vector_search".into(),
                    reason: format!("Qdrant health check failed: {e}"),
                },
                serde_json::json!({}),
            ))
    }
}

/// Generate a text description for a table (used for embedding).
fn table_to_text(schema: &crate::schema::TableSchema) -> String {
    let cols: Vec<String> = schema.columns.iter()
        .map(|c| format!("{} {}", c.name, crate::schema::data_type_name(c.data_type)))
        .collect();
    let desc = schema.description.as_ref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    format!("Table: {}{}\nColumns: {}", schema.name, desc, cols.join(", "))
}

/// Generate a text description for a column (used for embedding).
fn column_to_text(table: &str, column: &crate::schema::ColumnSchema) -> String {
    let desc = column.description.as_ref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    format!("Column: {}.{} {}", table, column.name, crate::schema::data_type_name(column.data_type))
}
```

- [ ] **Step 3: 在 `mod.rs` 中添加模块声明**

```rust
pub mod builder;
pub mod skill;

#[cfg(feature = "vector-search")]
pub mod schema_index;
```

- [ ] **Step 4: 验证编译**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core --no-default-features  # 无向量检索依赖
cargo check -p vlorql-core --features vector-search  # 有向量检索依赖
```

- [ ] **Step 5: 提交**

```bash
git add crates/vlorql-core/Cargo.toml crates/vlorql-core/src/prompt/schema_index.rs crates/vlorql-core/src/prompt/mod.rs
git commit -m "feat(schema): add SchemaIndexer skeleton with Qdrant feature gate"
```

---

### Task 2: SchemaIndexer 完整实现 — 索引 + 检索

**Files:**
- Modify: `crates/vlorql-core/src/prompt/schema_index.rs`

**Interfaces:**
- Consumes: `SchemaSnapshot`, `TableSchema`, `ColumnSchema`, `data_type_name()`
- Produces: `index_schema()` 完整实现, `search()` 完整实现

- [ ] **Step 1: 实现 `index_schema()`**

```rust
pub async fn index_schema(&self, schema: &SchemaSnapshot) -> Result<(), VlorQLError> {
    use qdrant_client::qdrant::{
        CreateCollectionBuilder, Distance, VectorParamsBuilder,
        PointStructBuilder, UpsertPointsBuilder,
    };
    use qdrant_client::Payload;

    // 1. 创建 collection（如果不存在）
    let collections = self.client.list_collections().await
        .map_err(|e| qdrant_error(e))?;
    let exists = collections.collections.iter()
        .any(|c| c.name == self.collection_name);
    if !exists {
        let params = VectorParamsBuilder::default()
            .size(384)  // all-MiniLM-L6-v2 dimension
            .distance(Distance::Cosine);
        self.client.create_collection(
            CreateCollectionBuilder::new(self.collection_name.clone())
                .vectors_config(params)
        ).await.map_err(|e| qdrant_error(e))?;
    }

    // 2. 为每个表/列生成文本和 payload
    let mut points = Vec::new();
    let mut id: u64 = 0;
    for table in &schema.tables {
        // Table point
        let text = table_to_text(table);
        let embedding = embed_text(&text).await?;
        points.push(
            PointStructBuilder::new(id, embedding)
                .payload(Payload::try_from(
                    serde_json::json!({"type": "table", "name": table.name, "text": text})
                ).unwrap())
        );
        id += 1;

        // Column points
        for column in &table.columns {
            let text = column_to_text(&table.name, column);
            let embedding = embed_text(&text).await?;
            points.push(
                PointStructBuilder::new(id, embedding)
                    .payload(Payload::try_from(
                        serde_json::json!({"type": "column", "table": table.name, "column": column.name, "text": text})
                    ).unwrap())
            );
            id += 1;
        }
    }

    // 3. Upsert points
    self.client.upsert_points(
        UpsertPointsBuilder::new(self.collection_name.clone(), points)
    ).await.map_err(|e| qdrant_error(e))?;

    Ok(())
}
```

- [ ] **Step 2: 实现 `search()`**

```rust
pub async fn search(&self, question: &str, top_k: u64) -> Result<Vec<String>, VlorQLError> {
    use qdrant_client::qdrant::{
        QueryPointsBuilder, SearchResponse,
    };

    let query_vector = embed_text(question).await?;

    let result = self.client.query(
        QueryPointsBuilder::new(self.collection_name.clone())
            .query(query_vector)
            .limit(top_k)
            .with_payload(true)
    ).await.map_err(|e| qdrant_error(e))?;

    // Extract unique table names from results
    let mut tables: Vec<String> = Vec::new();
    for point in result.result {
        if let Some(payload) = point.payload {
            if let Some(name) = payload.get("name").and_then(|v| v.as_str()) {
                if !tables.contains(&name.to_owned()) {
                    tables.push(name.to_owned());
                }
            }
            if let Some(table) = payload.get("table").and_then(|v| v.as_str()) {
                if !tables.contains(&table.to_owned()) {
                    tables.push(table.to_owned());
                }
            }
        }
    }
    Ok(tables)
}
```

- [ ] **Step 3: 添加 `embed_text()` 和 `qdrant_error()` 辅助函数**

```rust
/// Embed text to a vector. Uses a simple local embedding or API.
/// For now, uses OpenAI-compatible embedding API as the default.
async fn embed_text(text: &str) -> Result<Vec<f32>, VlorQLError> {
    // TODO: Integration with local embedding model or API
    // Placeholder: returns a zero vector (will be replaced with real embedding)
    Ok(vec![0.0f32; 384])
}

fn qdrant_error(e: impl std::fmt::Display) -> VlorQLError {
    VlorQLError::config(
        crate::errors::ConfigErrorKind::ConfigFileError {
            path: "vector_search".into(),
            reason: format!("Qdrant error: {e}"),
        },
        serde_json::json!({}),
    )
}
```

- [ ] **Step 4: 验证编译**

```bash
cargo check -p vlorql-core --features vector-search
```

- [ ] **Step 5: 提交**

```bash
git add crates/vlorql-core/src/prompt/schema_index.rs
git commit -m "feat(schema): implement SchemaIndexer index_schema + search"
```

---

### Task 3: PromptBuilder 集成 + VlorQlBuilder 开关

**Files:**
- Modify: `crates/vlorql-core/src/prompt/builder.rs`
- Modify: `crates/vlorql/src/lib.rs`（VlorQlBuilder 添加开关）

**Interfaces:**
- Consumes: `SchemaIndexer`, `SchemaSnapshot`
- Produces: `PromptBuilder.vector_search: bool`, `PromptBuilder.schema_indexer: Option<Arc<SchemaIndexer>>`
- Produces: `VlorQlBuilder::with_vector_search(bool) → Self`

- [ ] **Step 1: PromptBuilder 添加字段**

```rust
pub struct PromptBuilder {
    pub schema: Arc<SchemaSnapshot>,
    pub dialect: DialectProfile,
    pub policy: PolicyConfig,
    pub skill: Option<Arc<PromptSkill>>,
    pub include_examples: bool,
    pub vector_search: bool,
    pub schema_indexer: Option<Arc<SchemaIndexer>>,
}
```

- [ ] **Step 2: 修改 `filter_relevant_tables()`**

```rust
pub fn filter_relevant_tables(&self, user_question: &str) -> Vec<String> {
    if self.schema.tables.is_empty() {
        return Vec::new();
    }

    // Vector search path (when enabled)
    #[cfg(feature = "vector-search")]
    if self.vector_search {
        if let Some(indexer) = &self.schema_indexer {
            match indexer.search(user_question, 5).await {
                Ok(tables) if !tables.is_empty() => {
                    let set: HashSet<String> = tables.into_iter().collect();
                    let expanded = self.expand_foreign_key_neighbors(&set);
                    let result: Vec<String> = self.schema.tables.iter()
                        .filter(|t| expanded.contains(&t.name))
                        .map(|t| t.name.clone())
                        .collect();
                    if !result.is_empty() {
                        return result;
                    }
                }
                _ => { /* fall through to TF-IDF */ }
            }
        }
    }

    // Existing TF-IDF path (default + fallback)
    self.filter_relevant_tables_tfidf(user_question)
}

/// Extract TF-IDF logic to a separate method so the vector search path
/// can fall through to it.
fn filter_relevant_tables_tfidf(&self, user_question: &str) -> Vec<String> {
    // ... existing code from filter_relevant_tables ...
}
```

注意：`filter_relevant_tables` 需要改为 async。当前它是 sync 方法，调用链需要调整。

- [ ] **Step 3: VlorQlBuilder 添加 `with_vector_search()`**

```rust
// 在 vlorql/src/lib.rs 的 VlorQlBuilder 中
pub fn with_vector_search(mut self, enabled: bool) -> Self {
    self.vector_search = enabled;
    self
}
```

- [ ] **Step 4: 提交**

```bash
git add crates/vlorql-core/src/prompt/builder.rs crates/vlorql/src/lib.rs
git commit -m "feat(schema): integrate SchemaIndexer into PromptBuilder with vector_search switch"
```
