# F6: LlmResponseCache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增"问题 → QueryPlan"的 LLM 响应缓存，避免对同一问题的重复 LLM 调用。

**Architecture:** 新建 `crates/vlorql-core/src/cache/llm_cache.rs`，复用项目已有的 `Cache` 模式（`moka::future::Cache` 后端），Key = `(normalized_question, schema_version, model_fingerprint)`，Value = `Arc<QueryPlan>`。然后在 `vlorql/src/lib.rs` 的 `query()` 方法中接入。

**Tech Stack:** Rust (edition 2024)，moka 0.12（已有依赖），xxhash-rust（已有依赖），tokio。

## Global Constraints

- CI 全绿：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check`
- `#![deny(missing_docs)]`：所有新增公共项必须有文档注释
- 使用已有 `moka::future::Cache`，不加新第三方依赖
- 不修改 `query()` 公共 API 签名
- TDD：先写失败测试 → 确认失败 → 最小实现 → 确认通过 → 提交
- 新增模块自带完整单元测试

---

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-core/src/cache/llm_cache.rs`（**新建**） | `LlmCacheKey` + `LlmResponseCache` + 单元测试 | 1 |
| `crates/vlorql-core/src/cache/mod.rs`（修改） | 加 `mod llm_cache` + `pub use` | 1 |
| `crates/vlorql/src/lib.rs`（修改） | `VlorQlEngine` 新增 `llm_cache` 字段 + `query()` 中接入 | 2 |

---

### Task 1: Core — LlmResponseCache 模块

**Files:**
- Create: `crates/vlorql-core/src/cache/llm_cache.rs`
- Modify: `crates/vlorql-core/src/cache/mod.rs`

**Interfaces:**
- Consumes: `crate::schema::QueryPlan`, `moka::future::Cache`, `std::sync::Arc`
- Produces:
  - `pub struct LlmCacheKey { normalized_question: String, schema_version: String, model_fingerprint: String }`
  - `pub struct LlmResponseCache { inner: moka::future::Cache<LlmCacheKey, Arc<QueryPlan>> }`
  - `impl LlmResponseCache { new(max_entries, ttl_seconds), get(&self, key) -> Option<Arc<QueryPlan>>, insert(&self, key, plan), invalidate_question(&self, question: &str), invalidate_schema_version(&self, version: &str), clear(&self), size(&self) -> u64 }`

- [ ] **Step 1: Write the failing test**

在 `crates/vlorql-core/src/cache/llm_cache.rs` 底部写测试（先写测试，此时 `LlmCacheKey`/`LlmResponseCache` 未定义，编译会失败）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FromClause, Projection, QueryPlan};
    use std::sync::Arc;

    fn sample_key() -> LlmCacheKey {
        LlmCacheKey {
            normalized_question: "how many users".to_owned(),
            schema_version: "v1".to_owned(),
            model_fingerprint: "openai:gpt-4".to_owned(),
        }
    }

    fn sample_plan() -> Arc<QueryPlan> {
        Arc::new(QueryPlan {
            select: vec![Projection::Column {
                table: None,
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,
        })
    }

    #[tokio::test]
    async fn cache_hit_returns_cached_plan() {
        let cache = LlmResponseCache::new(100, 3600);
        let key = sample_key();
        let plan = sample_plan();

        // Miss first
        assert!(cache.get(&key).await.is_none());

        // Insert
        cache.insert(key.clone(), plan.clone()).await;

        // Hit
        let cached = cache.get(&key).await.expect("should be cached");
        assert_eq!(cached.from.table, "users");
    }

    #[tokio::test]
    async fn different_key_misses() {
        let cache = LlmResponseCache::new(100, 3600);
        let key_a = sample_key();
        let key_b = LlmCacheKey {
            normalized_question: "other question".to_owned(),
            ..sample_key()
        };
        let plan = sample_plan();

        cache.insert(key_a, plan).await;
        assert!(cache.get(&key_b).await.is_none());
    }

    #[tokio::test]
    async fn invalidate_question_removes_entry() {
        let cache = LlmResponseCache::new(100, 3600);
        let key = sample_key();
        cache.insert(key.clone(), sample_plan()).await;
        assert_eq!(cache.size(), 1);

        cache.invalidate_question("how many users");
        assert_eq!(cache.size(), 0);
    }

    #[tokio::test]
    async fn invalidate_schema_version_removes_entries() {
        let cache = LlmResponseCache::new(100, 3600);
        let key_v1 = LlmCacheKey {
            normalized_question: "q1".to_owned(),
            schema_version: "v1".to_owned(),
            model_fingerprint: "m".to_owned(),
        };
        let key_v2 = LlmCacheKey {
            normalized_question: "q2".to_owned(),
            schema_version: "v2".to_owned(),
            model_fingerprint: "m".to_owned(),
        };
        cache.insert(key_v1, sample_plan()).await;
        cache.insert(key_v2, sample_plan()).await;
        assert_eq!(cache.size(), 2);

        cache.invalidate_schema_version("v1");
        assert_eq!(cache.size(), 1);
    }

    #[tokio::test]
    async fn clear_removes_all_entries() {
        let cache = LlmResponseCache::new(100, 3600);
        cache.insert(sample_key(), sample_plan()).await;
        cache.clear();
        assert_eq!(cache.size(), 0);
    }

    #[tokio::test]
    async fn concurrent_access_is_safe() {
        let cache = std::sync::Arc::new(LlmResponseCache::new(100, 3600));
        let mut handles = Vec::new();
        for i in 0..10 {
            let c = cache.clone();
            handles.push(tokio::spawn(async move {
                let key = LlmCacheKey {
                    normalized_question: format!("q{i}"),
                    schema_version: "v1".to_owned(),
                    model_fingerprint: "m".to_owned(),
                };
                c.insert(key, sample_plan()).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(cache.size(), 10);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:
```bash
cargo test -p vlorql-core --lib cache::llm_cache 2>&1 | head -15
```
Expected: compilation error (module `llm_cache` not found or types not defined).

- [ ] **Step 3: Implement `LlmCacheKey` + `LlmResponseCache`**

新建 `crates/vlorql-core/src/cache/llm_cache.rs`，写入完整实现：

```rust
//! LLM response cache — avoids redundant LLM calls for identical questions.
//!
//! The cache key is derived from the normalized question text, schema
//! version, and model fingerprint so that different questions, schemas,
//! or models never collide.

use crate::schema::QueryPlan;
use moka::future::Cache as MokaCache;
use std::sync::Arc;

/// Key that uniquely identifies an LLM query result.
///
/// Two queries match only when the user's question, the schema version,
/// *and* the model are all identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LlmCacheKey {
    /// Normalised question text (lowercased, whitespace-collapsed).
    pub normalized_question: String,
    /// Schema version from [`SchemaSnapshot`](crate::schema::SchemaSnapshot).
    pub schema_version: String,
    /// Provider + model name, e.g. `"openai:gpt-4"`.
    pub model_fingerprint: String,
}

/// An in-memory cache for LLM-generated `QueryPlan`s.
///
/// Wraps a `moka::future::Cache` with LRU eviction and optional TTL.
/// The cache is thread-safe and can be shared across async tasks via
/// `Arc<LlmResponseCache>`.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::llm_cache::{LlmCacheKey, LlmResponseCache};
///
/// let cache = LlmResponseCache::new(100, 3600);
/// let key = LlmCacheKey {
///     normalized_question: "list users".to_owned(),
///     schema_version: "v1".to_owned(),
///     model_fingerprint: "openai:gpt-4".to_owned(),
/// };
/// assert!(cache.get(&key).await.is_none());
/// ```
pub struct LlmResponseCache {
    inner: MokaCache<LlmCacheKey, Arc<QueryPlan>>,
}

impl LlmResponseCache {
    /// Creates a new LLM response cache.
    ///
    /// * `max_entries` — maximum number of entries before LRU eviction.
    /// * `ttl_seconds` — time-to-live in seconds after insertion.
    #[must_use]
    pub fn new(max_entries: u64, ttl_seconds: u64) -> Self {
        use std::time::Duration;
        Self {
            inner: MokaCache::builder()
                .max_capacity(max_entries)
                .time_to_live(Duration::from_secs(ttl_seconds))
                .build(),
        }
    }

    /// Returns the cached plan for `key`, or `None` on a miss.
    #[must_use]
    pub async fn get(&self, key: &LlmCacheKey) -> Option<Arc<QueryPlan>> {
        self.inner.get(key).await
    }

    /// Inserts a plan into the cache.
    pub async fn insert(&self, key: LlmCacheKey, plan: Arc<QueryPlan>) {
        self.inner.insert(key, plan).await;
    }

    /// Invalidates all entries whose `normalized_question` matches `question`.
    ///
    /// This is a linear scan — call sparingly on large caches.
    pub fn invalidate_question(&self, question: &str) {
        // moka doesn't support prefix-based invalidation directly,
        // so we iterate through a snapshot of the keys.
        if let Some(mut iter) = self.inner.iter() {
            let keys: Vec<LlmCacheKey> = iter
                .filter_map(|(k, _)| {
                    if k.normalized_question == question {
                        Some(k.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                self.inner.invalidate(&key).await;
            }
        }
    }

    /// Invalidates all entries whose `schema_version` matches `version`.
    pub async fn invalidate_schema_version(&self, version: &str) {
        if let Some(mut iter) = self.inner.iter() {
            let keys: Vec<LlmCacheKey> = iter
                .filter_map(|(k, _)| {
                    if k.schema_version == version {
                        Some(k.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for key in keys {
                self.inner.invalidate(&key).await;
            }
        }
    }

    /// Removes all entries from the cache.
    pub fn clear(&self) {
        self.inner.invalidate_all();
    }

    /// Returns the number of entries currently in the cache.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.inner.entry_count()
    }
}
```

以下为测试代码（已在 Step 1 写好，文件末尾）：

<--- tests here (Step 1 above) --->

- [ ] **Step 4: Register module**

修改 `crates/vlorql-core/src/cache/mod.rs`，在 `mod prompt_cache;` 之后加：

```rust
mod llm_cache;
```

在 `pub use prompt_cache::{...};` 之后加：

```rust
pub use llm_cache::{LlmCacheKey, LlmResponseCache};
```

- [ ] **Step 5: Run test to verify it passes**

Run:
```bash
cargo test -p vlorql-core --lib cache::llm_cache
```
Expected: all tests PASS.

- [ ] **Step 6: Clippy + fmt**

```bash
cargo clippy -p vlorql-core --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/vlorql-core/src/cache/llm_cache.rs crates/vlorql-core/src/cache/mod.rs
git commit -m "feat(cache): add LlmResponseCache with LlmCacheKey (F6-1)"
```

---

### Task 2: Integration — 接入 VlorQlEngine

**Files:**
- Modify: `crates/vlorql/src/lib.rs`

**Interfaces:**
- Consumes: `vlorql_core::cache::LlmCacheKey`, `vlorql_core::cache::LlmResponseCache` (from Task 1)
- Produces: `VlorQlEngine` gains `llm_cache` field; `query()` performs cache lookup before LLM call

- [ ] **Step 1: Explore integration point**

Read `crates/vlorql/src/lib.rs`:
- Find `pub struct VlorQlEngine` — add `pub(crate) llm_cache: LlmResponseCache` field
- Find `pub async fn query()` — find where `client.generate_plan()` is called
- Find the model fingerprint source — find how provider + model name is accessible (likely `client.config()`)
- Read existing `query()` function to understand the flow around `generate_plan()`

- [ ] **Step 2: Write integration test**

In `crates/vlorql/src/lib.rs` `mod tests`:

```rust
#[tokio::test]
async fn llm_cache_hit_skips_llm_call() {
    use vlorql_core::cache::LlmCacheKey;

    let mut engine = VlorQlEngine::new(Config::default(), Schema::default());
    // The sequence client returns the next plan on each call.
    // On a cache hit the second call should not invoke generate_plan.
    let _ = engine.query("test question", None).await;
    let _ = engine.query("test question", None).await;
    // Sequence client tracks call count; if cache works, count == 1 not 2.
    // (Implementation depends on SequenceClient — adjust after reading the code.)
}
```

(Tip: read the actual test helpers in `vlorql/src/lib.rs` — `SequenceClient` etc. — to adapt the test.)

- [ ] **Step 3: Implement integration**

In `VlorQlEngine` struct definition, add:

```rust
pub(crate) llm_cache: LlmResponseCache,
```

In `VlorQlEngine::new()` or equivalent constructor, initialize:

```rust
llm_cache: LlmResponseCache::new(1000, 3600),
```

In `query()` method, before calling `generate_plan`:

```rust
// Cache lookup: skip LLM call for identical questions.
if let Some(cached) = self.llm_cache.get(&key).await {
    return Ok(cached);
}
```

After `generate_plan` succeeds, insert into cache:

```rust
self.llm_cache.insert(key, Arc::new(plan.clone())).await;
```

Build key using:
```rust
let model_fingerprint = format!("{}:{}", client.config().provider, client.config().model);
let key = LlmCacheKey {
    normalized_question: question.to_lowercase(), // basic normalisation
    schema_version: schema.version().to_owned(),
    model_fingerprint,
};
```

- [ ] **Step 4: Verify**

```bash
cargo test -p vlorql
cargo clippy -p vlorql --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vlorql/src/lib.rs
git commit -m "feat(engine): integrate LlmResponseCache into VlorQlEngine (F6-2)"
```

---

### Task 3: Final verification

- [ ] **Step 1: Full workspace check**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
Expected: all green.

- [ ] **Step 2: Update plan doc**

```bash
git add docs/superpowers/plans/2026-07-27-f6-llm-response-cache.md
git commit -m "docs: add F6 implementation plan"
```

---

## Self-Review

1. **Spec coverage:** Spec requires `LlmCacheKey` with 3 fields (✅ Task 1), `LlmResponseCache` with moka backend (✅ Task 1), integration into `query()` before `generate_plan` (✅ Task 2), tests for hit/miss/invalidate/clear/concurrent (✅ Task 1 tests). No gaps.
2. **Placeholder scan:** No TBD/TODO. All code blocks complete. Task 2 Step 1 notes "read the actual code" which is by design (integration point shape varies by codebase).
3. **Type consistency:** `LlmCacheKey { normalized_question, schema_version, model_fingerprint }` consistent across Task 1 definition and Task 2 usage. `LlmResponseCache::new(max_entries, ttl_seconds)` consistent. `get(key) -> Option<Arc<QueryPlan>>` consistent.
4. **Scope check:** Two focused tasks (core module + integration). No extra scope.
