# Schema Grounding Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the partially-implemented Schema Grounding integration — fix build errors, wire vector search end-to-end through VlorQlBuilder → VlorQl → PromptBuilder, add real OpenAI embeddings, add integration tests.

**Architecture:** Store `vector_search` flag and `Option<Arc<SchemaIndexer>>` on VlorQl facade, forward to PromptBuilder on query(). `filter_relevant_tables()` merges vector search results with existing TF-IDF using `tokio::runtime::Handle::try_current().block_on()` to bridge async→sync. Embedding via OpenAI API behind `vector-search` feature gate.

**Tech Stack:** Rust, `qdrant-client` (optional), `reqwest`, OpenAI Embeddings API, `tokio`

## Global Constraints

- Vector search defaults to false; opt-in via `with_vector_search(true)`
- No new dependencies for users not enabling `vector-search` feature
- Qdrant must be deployed separately by the user
- Vector search failures silently fall back to TF-IDF (no error propagation to caller)
- Async→sync bridge via `tokio::runtime::Handle::try_current().block_on()` — safe because all callers run in tokio runtime
- Embedding: OpenAI `text-embedding-3-small`, API key from `OPENAI_API_KEY` env var, cached per unique text

---

### Task 1: Fix PromptBuilder Feature Gate + Add Setters

**Files:**
- Modify: `crates/vlorql-core/src/prompt/builder.rs`

**Interfaces:**
- Produces: `PromptBuilder::with_vector_search(bool) -> Self`
- Produces: `PromptBuilder::with_schema_indexer(Arc<SchemaIndexer>) -> Self` (cfg-gated)
- Produces: `PromptBuilder` fields `vector_search: bool` and `#[cfg(feature = "vector-search")] schema_indexer: Option<Arc<SchemaIndexer>>`

- [ ] **Step 1: Gate the `schema_indexer` field with `#[cfg(feature = "vector-search")]`**

```rust
// Line 61-64: replace the unconditional field with a cfg-gated version
    /// Enables vector-based schema retrieval via Qdrant (default: false).
    vector_search: bool,
    /// Optional schema indexer for semantic table/column search.
    #[cfg(feature = "vector-search")]
    schema_indexer: Option<Arc<crate::prompt::schema_index::SchemaIndexer>>,
```

- [ ] **Step 2: Gate the field initialization in `PromptBuilder::new()`**

```rust
    // Lines 72-81: add cfg on schema_indexer
    fn new(schema: Arc<SchemaSnapshot>, dialect: DialectProfile, policy: PolicyConfig) -> Self {
        let reverse_fk_index = build_reverse_fk_index(&schema);
        Self {
            schema,
            dialect,
            policy_hash: hash_policy(&policy),
            policy,
            include_examples: true,
            skill: None,
            reverse_fk_index,
            vector_search: false,
            #[cfg(feature = "vector-search")]
            schema_indexer: None,
        }
    }
```

- [ ] **Step 3: Add `with_vector_search()` setter (after `with_examples()`, before `build_system_prompt`)**

```rust
    /// Enables or disables vector-based schema retrieval via Qdrant.
    ///
    /// When enabled, the prompt builder will use semantic vector search
    /// to find relevant tables for the user question. Default is `false`.
    #[must_use]
    pub fn with_vector_search(mut self, enabled: bool) -> Self {
        self.vector_search = enabled;
        self
    }
```

- [ ] **Step 4: Add `with_schema_indexer()` setter (cfg-gated)**

```rust
    /// Supplies a SchemaIndexer for semantic table/column vector search.
    #[cfg(feature = "vector-search")]
    #[must_use]
    pub fn with_schema_indexer(mut self, indexer: Arc<crate::prompt::schema_index::SchemaIndexer>) -> Self {
        self.schema_indexer = Some(indexer);
        self
    }
```

- [ ] **Step 5: Verify compilation**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core --no-default-features
cargo check -p vlorql-core --features vector-search
```

- [ ] **Step 6: Commit**

```bash
git add crates/vlorql-core/src/prompt/builder.rs
git commit -m "fix(schema): feature-gate schema_indexer field, add with_vector_search/with_schema_indexer setters"
```

---

### Task 2: Wire VlorQlBuilder → VlorQl → PromptBuilder

**Files:**
- Modify: `crates/vlorql/src/builder.rs`
- Modify: `crates/vlorql/src/lib.rs`

**Interfaces:**
- Consumes: `PromptBuilder::with_vector_search()`, `PromptBuilder::with_schema_indexer()`
- Produces: `VlorQlBuilder::with_vector_search(bool) -> Self` (cfg-gated)
- Produces: `VlorQlBuilder::with_schema_indexer(Arc<SchemaIndexer>) -> Self` (cfg-gated)
- Produces: `VlorQl.vector_search: bool`, `VlorQl.schema_indexer: Option<Arc<SchemaIndexer>>`

- [ ] **Step 1: Gate `vector_search` field in `VlorQlBuilder`**

```rust
// builder.rs line 66: add cfg
#[cfg(feature = "vector-search")]
    vector_search: bool,
```

Also gate the `Default` impl (line 90):
```rust
#[cfg(feature = "vector-search")]
            vector_search: false,
```

Also gate existing `with_vector_search()` method (line 318):
```rust
#[cfg(feature = "vector-search")]
    #[must_use]
    pub fn with_vector_search(mut self, enabled: bool) -> Self {
        self.vector_search = enabled;
        self
    }
```

- [ ] **Step 2: Add `with_schema_indexer()` to `VlorQlBuilder`**

```rust
// After with_vector_search, before build():
    /// Supplies a SchemaIndexer for semantic schema retrieval via Qdrant.
    #[cfg(feature = "vector-search")]
    #[must_use]
    pub fn with_schema_indexer(mut self, indexer: Arc<vlorql_core::prompt::schema_index::SchemaIndexer>) -> Self {
        self.schema_indexer = Some(indexer);
        self
    }
```

Also add the field:
```rust
// After vector_search: bool line:
    #[cfg(feature = "vector-search")]
    schema_indexer: Option<Arc<vlorql_core::prompt::schema_index::SchemaIndexer>>,
```

And its Default:
```rust
#[cfg(feature = "vector-search")]
            schema_indexer: None,
```

- [ ] **Step 3: Pass `vector_search` and `schema_indexer` in `VlorQlBuilder::build()`**

```rust
// Near the end of build(), before Ok(VlorQl { ... }):
        #[cfg(feature = "vector-search")]
        let vector_search = self.vector_search;
        #[cfg(not(feature = "vector-search"))]
        let vector_search = false;

        #[cfg(feature = "vector-search")]
        let schema_indexer = self.schema_indexer;
        #[cfg(not(feature = "vector-search"))]
        let schema_indexer: Option<Arc<vlorql_core::prompt::schema_index::SchemaIndexer>> = None;
```

Add fields to the `VlorQl { }` constructor:
```rust
            vector_search,
            schema_indexer,
```

- [ ] **Step 4: Add fields to `VlorQl` struct + Debug impl**

```rust
// lib.rs after executor field:
    #[cfg(feature = "vector-search")]
    vector_search: bool,
    #[cfg(feature = "vector-search")]
    schema_indexer: Option<Arc<vlorql_core::prompt::schema_index::SchemaIndexer>>,
```

Add to Debug impl:
```rust
            .field("vector_search", &self.vector_search)
            .field("has_schema_indexer", &self.schema_indexer.is_some())
```

- [ ] **Step 5: Forward to PromptBuilder in `query()` method**

```rust
// lib.rs line 228-232: after PromptBuilder::new(...), add:
            #[allow(unused_mut)]
            let mut prompt_builder = PromptBuilder::new(
                Arc::clone(&schema),
                self.dialect.clone(),
                self.policy.clone(),
            );
            #[cfg(feature = "vector-search")]
            {
                prompt_builder = prompt_builder
                    .with_vector_search(self.vector_search);
                if let Some(ref indexer) = self.schema_indexer {
                    prompt_builder = prompt_builder.with_schema_indexer(Arc::clone(indexer));
                    // Index schema on first query
                    indexer.index_schema(&schema).await.ok();
                }
            }
```

Also add the import:
```rust
// At top of lib.rs:
use vlorql_core::prompt::schema_index::SchemaIndexer;
```
This import needs a cfg gate:
```rust
#[cfg(feature = "vector-search")]
use vlorql_core::prompt::schema_index::SchemaIndexer;
```

- [ ] **Step 6: Add `vector-search` feature propagation to `vlorql/Cargo.toml`**

```toml
[features]
default = ["executor-postgres", "executor-mysql", "executor-sqlite"]
vector-search = ["vlorql-core/vector-search"]
```

- [ ] **Step 7: Verify compilation**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql --no-default-features
cargo check -p vlorql --features vector-search
```

- [ ] **Step 8: Commit**

```bash
git add crates/vlorql/src/builder.rs crates/vlorql/src/lib.rs crates/vlorql/Cargo.toml
git commit -m "feat(schema): wire vector_search + schema_indexer through VlorQlBuilder → VlorQl → PromptBuilder"
```

---

### Task 3: Real Embedding via OpenAI API

**Files:**
- Modify: `crates/vlorql-core/src/prompt/schema_index.rs`
- Modify: `crates/vlorql-core/Cargo.toml`

**Interfaces:**
- Consumes: `OPENAI_API_KEY` env var
- Produces: `embed_text(text: &str) -> Result<Vec<f32>, VlorQLError>` (real implementation)

- [ ] **Step 1: Add `reqwest` to `vlorql-core/Cargo.toml` behind `vector-search` feature**

```toml
vector-search = ["qdrant-client", "reqwest"]
```

No change needed to `[dependencies]` section since `reqwest` is already a workspace dep and can be used as optional:
```toml
reqwest = { workspace = true, optional = true }
```

- [ ] **Step 2: Replace `embed_text()` placeholder with real OpenAI API call**

```rust
use std::collections::HashMap;
use std::sync::Mutex;

// Simple in-memory cache: input text → embedding vector
static EMBEDDING_CACHE: once_cell::sync::Lazy<Mutex<HashMap<String, Vec<f32>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// Embed text using OpenAI text-embedding-3-small API.
/// Caches results per unique input text to avoid re-embedding during schema indexing.
async fn embed_text(text: &str) -> Result<Vec<f32>, VlorQLError> {
    // Check cache
    {
        let cache = EMBEDDING_CACHE.lock().unwrap();
        if let Some(cached) = cache.get(text) {
            return Ok(cached.clone());
        }
    }

    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "OPENAI_API_KEY".into(),
                reason: "OPENAI_API_KEY environment variable not set".into(),
            },
            serde_json::json!({}),
        ))?;

    let client = reqwest::Client::new();
    let resp = client.post("https://api.openai.com/v1/embeddings")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": text,
        }))
        .send()
        .await
        .map_err(|e| VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "vector_search".into(),
                reason: format!("OpenAI embedding request failed: {e}"),
            },
            serde_json::json!({}),
        ))?;

    let body: serde_json::Value = resp.json().await
        .map_err(|e| VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "vector_search".into(),
                reason: format!("OpenAI embedding parse failed: {e}"),
            },
            serde_json::json!({}),
        ))?;

    let vector: Vec<f32> = body["data"][0]["embedding"]
        .as_array()
        .ok_or_else(|| VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "vector_search".into(),
                reason: "OpenAI embedding response missing embedding field".into(),
            },
            serde_json::json!({}),
        ))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();

    // Cache the result
    {
        let mut cache = EMBEDDING_CACHE.lock().unwrap();
        cache.insert(text.to_owned(), vector.clone());
    }

    Ok(vector)
}
```

- [ ] **Step 3: Update dimension constant (OpenAI text-embedding-3-small = 512, but collection created with 384)**

In `index_schema()` and the collection creation, the dimension should match the model:
```rust
const EMBEDDING_DIM: u64 = 512;
```

Update `create_collection` call:
```rust
let params = VectorParamsBuilder::new(EMBEDDING_DIM, Distance::Cosine);
```

- [ ] **Step 4: Add `once_cell` to `vlorql-core/Cargo.toml`**

This is needed for the static embedding cache. Add to workspace deps if not present:
Check workspace Cargo.toml first — if no `once_cell`, use `std::sync::LazyLock` (available in Rust 1.80+, project uses 1.85+). Actually with MSRV 1.85+, we can use `std::sync::LazyLock`:

```rust
use std::sync::LazyLock;

static EMBEDDING_CACHE: LazyLock<Mutex<HashMap<String, Vec<f32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
```

No extra dependency needed.

- [ ] **Step 5: Verify compilation**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core --features vector-search
```

- [ ] **Step 6: Commit**

```bash
git add crates/vlorql-core/src/prompt/schema_index.rs crates/vlorql-core/Cargo.toml
git commit -m "feat(schema): replace zero-vector placeholder with OpenAI embedding API"
```

---

### Task 4: Integrate Vector Search into filter_relevant_tables()

**Files:**
- Modify: `crates/vlorql-core/src/prompt/builder.rs`

**Interfaces:**
- Consumes: `PromptBuilder.vector_search`, `PromptBuilder.schema_indexer`
- Produces: `filter_relevant_tables()` now uses vector search when enabled

- [ ] **Step 1: Add vector search branch to `filter_relevant_tables()`**

Add at the top of `filter_relevant_tables()`, after the empty-schema guard and before TF-IDF:

```rust
    // Vector search path (when enabled and configured)
    #[cfg(feature = "vector-search")]
    if self.vector_search {
        if let Some(ref indexer) = self.schema_indexer {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                match runtime.block_on(indexer.search(user_question, 10)) {
                    Ok(tables) if !tables.is_empty() => {
                        // Check if vector search gave us enough coverage
                        let set: HashSet<String> = tables.into_iter().collect();
                        let expanded = self.expand_foreign_key_neighbors(&set);
                        let result: Vec<String> = self.schema.tables.iter()
                            .filter(|t| expanded.contains(&t.name))
                            .map(|t| t.name.clone())
                            .collect();
                        if !result.is_empty() {
                            info!("vector_search: found {} relevant tables", result.len());
                            return result;
                        }
                        // Empty result from vector search: fall through to TF-IDF
                    }
                    Ok(_) => { /* empty vector results, fall through */ }
                    Err(e) => {
                        tracing::warn!(error = %e, "vector_search failed, falling back to TF-IDF");
                        // Fall through to TF-IDF
                    }
                }
            }
        }
    }
```

Add import:
```rust
use tracing::info;
```
(Already imported via `tracing` in dependencies, check if `info` is used — if not, add to the import.)

- [ ] **Step 2: Ensure TF-IDF always runs as default (no code change needed — the vector branch returns early or falls through)**

The existing TF-IDF code stays exactly as-is.

- [ ] **Step 3: Verify compilation**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo check -p vlorql-core --no-default-features
cargo check -p vlorql-core --features vector-search
cargo check -p vlorql --no-default-features
cargo check -p vlorql --features vector-search
```

- [ ] **Step 4: Run existing tests to ensure no regression**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo test -p vlorql-core --no-default-features
```

- [ ] **Step 5: Commit**

```bash
git add crates/vlorql-core/src/prompt/builder.rs
git commit -m "feat(schema): integrate vector search into filter_relevant_tables with TF-IDF fallback"
```

---

### Task 5: Add Tests

**Files:**
- Modify: `crates/vlorql-core/src/prompt/mod.rs` (add tests in the test module)
- Maybe create: `crates/vlorql-core/tests/` integration test

**Interfaces:**
- Tests for: vector search returns relevant tables, merge with TF-IDF, disabled fallback, build with vector search

- [ ] **Step 1: Add unit test for vector search returning relevant tables**

In `extra_tests` module in `mod.rs`:

```rust
#[cfg(feature = "vector-search")]
#[test]
fn vector_search_returns_relevant_tables() {
    // Create SchemaIndexer connected to a real Qdrant instance
    // (requires Qdrant on localhost:6334)
    let rt = tokio::runtime::Runtime::new().unwrap();
    let indexer = rt.block_on(
        vlorql_core::prompt::schema_index::SchemaIndexer::connect("http://localhost:6334")
    );

    let indexer = match indexer {
        Ok(idx) => {
            // Index the test schema
            let _ = rt.block_on(idx.index_schema(&*non_empty_schema()));
            Arc::new(idx)
        }
        Err(_) => {
            eprintln!("Skipping test: Qdrant not available on localhost:6334");
            return;
        }
    };

    let builder = PromptBuilder::new(
        non_empty_schema(),
        non_empty_dialect(),
        non_empty_policy(),
    )
    .with_vector_search(true)
    .with_schema_indexer(indexer);

    let tables = builder.filter_relevant_tables("Show me customer purchases");
    // Should include "orders" (customer purchase history)
    assert!(tables.contains(&"orders".to_owned()),
        "vector search should find orders table: got {tables:?}");
}
```

- [ ] **Step 2: Add unit test for disabled vector search fallback**

```rust
#[test]
fn vector_search_disabled_falls_back_to_tfidf() {
    let builder = PromptBuilder::new(
        non_empty_schema(),
        non_empty_dialect(),
        non_empty_policy(),
    )
    .with_vector_search(false);  // no indexer needed

    // Should still work via TF-IDF
    let tables = builder.filter_relevant_tables("Show me customer purchases");
    assert!(tables.contains(&"orders".to_owned()),
        "TF-IDF fallback should find orders table: got {tables:?}");
}
```

- [ ] **Step 3: Run tests**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo test -p vlorql-core --no-default-features
cargo test -p vlorql-core --features vector-search
```

- [ ] **Step 4: Commit**

```bash
git add crates/vlorql-core/src/prompt/mod.rs
git commit -m "test(schema): add vector search unit tests"
```

---

### Task 6: Final Verification + Full Build

- [ ] **Step 1: Full workspace build with all features**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
cargo build --all-features
cargo test --all-features
cargo clippy --all-features -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 2: Clear the uncommitted working tree changes (they're all superseded by these tasks)**

```bash
cd /home/vlor/projects/rust_projects/VlorQl
git diff --stat  # should show only our new changes
```

- [ ] **Step 3: Final commit (if last task not committed) or verify state**

```bash
git status
```
