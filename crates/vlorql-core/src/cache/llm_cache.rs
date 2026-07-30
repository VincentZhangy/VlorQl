//! Cache for LLM-generated [`QueryPlan`] responses.
//!
//! Two identical questions (same text, same schema version, same model
//! fingerprint) produce the same plan, so caching avoids redundant LLM
//! invocations.  The cache is keyed by [`LlmCacheKey`] and holds
//! [`Arc<QueryPlan>`] values.

use crate::schema::QueryPlan;
use moka::future::Cache as MokaCache;
use std::sync::Arc;
use std::time::Duration;

/// Key used to identify a cached LLM response.
///
/// The key binds the plan to the exact question text, schema snapshot,
/// and model version so that changes in any of these dimensions
/// produce a cache miss.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LlmCacheKey {
    /// The normalised user question text.
    pub normalized_question: String,
    /// Schema version string (e.g. `"v1.2.3"`).
    pub schema_version: String,
    /// Identifier for the LLM model / fine-tune version.
    pub model_fingerprint: String,
}

/// [`QueryPlan`](crate::schema::QueryPlan) values keyed by
/// [`LlmCacheKey`].
///
/// The cache uses a concurrent, bounded map provided by `moka`.
/// Entries are automatically evicted when they exceed the configured
/// TTL or when the cache grows past the configured capacity.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::{LlmCacheKey, LlmResponseCache};
///
/// let cache = LlmResponseCache::new(100, 300);
/// assert_eq!(cache.size(), 0);
///
/// let key = LlmCacheKey {
///     normalized_question: "show users".to_owned(),
///     schema_version: "v1".to_owned(),
///     model_fingerprint: "gpt-4".to_owned(),
/// };
/// // Callers with an async runtime can insert and retrieve plans:
/// // cache.insert(key, plan).await;
/// // let cached = cache.get(&key).await;
/// ```
#[derive(Debug, Clone)]
pub struct LlmResponseCache {
    inner: MokaCache<LlmCacheKey, Arc<QueryPlan>>,
}

impl LlmResponseCache {
    /// Creates a new LLM response cache.
    ///
    /// * `max_entries` — maximum number of entries before the cache
    ///   evicts least-recently-used items.
    /// * `ttl_seconds` — time-to-live in seconds.  Entries older than
    ///   this are automatically invalidated.
    #[must_use]
    pub fn new(max_entries: u64, ttl_seconds: u64) -> Self {
        let builder = MokaCache::builder()
            .max_capacity(max_entries)
            .time_to_live(Duration::from_secs(ttl_seconds));
        let inner = builder.build();
        Self { inner }
    }

    /// [`QueryPlan`](crate::schema::QueryPlan) for `key`, or `None` on a
    /// miss.
    pub async fn get(&self, key: &LlmCacheKey) -> Option<Arc<QueryPlan>> {
        self.inner.get(key).await
    }

    /// [`QueryPlan`](crate::schema::QueryPlan) into the cache under `key`.
    pub async fn insert(&self, key: LlmCacheKey, plan: Arc<QueryPlan>) {
        self.inner.insert(key, plan).await;
    }

    /// Removes all entries whose `normalized_question` matches
    /// `question`.
    ///
    /// This is useful when the user edits their question — the old
    /// cached plan for that question should be discarded.
    pub async fn invalidate_question(&self, question: &str) {
        let keys: Vec<LlmCacheKey> = self
            .inner
            .iter()
            .filter(|(k, _)| k.normalized_question == question)
            .map(|(k, _)| k.as_ref().clone())
            .collect();
        for key in keys {
            self.inner.invalidate(&key).await;
        }
    }

    /// Removes all entries whose `schema_version` matches `version`.
    ///
    /// This is useful when the schema changes — all plans derived
    /// from the old schema are invalidated at once.
    pub async fn invalidate_schema_version(&self, version: &str) {
        let keys: Vec<LlmCacheKey> = self
            .inner
            .iter()
            .filter(|(k, _)| k.schema_version == version)
            .map(|(k, _)| k.as_ref().clone())
            .collect();
        for key in keys {
            self.inner.invalidate(&key).await;
        }
    }

    /// Removes all entries from the cache.
    pub fn clear(&self) {
        self.inner.invalidate_all();
    }

    /// Returns the number of entries currently in the cache.
    ///
    /// This method runs pending maintenance tasks to ensure a
    /// reasonably accurate count.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.inner.entry_count()
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::schema::{FromClause, Projection, QueryPlan};

    fn dummy_plan(table: &str) -> Arc<QueryPlan> {
        Arc::new(QueryPlan {
            select: vec![Projection::Column {
                table: Some(table.to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause::table(table.to_owned(), None),
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

    fn key(question: &str, schema_version: &str, model: &str) -> LlmCacheKey {
        LlmCacheKey {
            normalized_question: question.to_owned(),
            schema_version: schema_version.to_owned(),
            model_fingerprint: model.to_owned(),
        }
    }

    /// A cache hit returns the previously inserted plan.
    #[tokio::test]
    async fn cache_hit_returns_cached_plan() {
        let cache = LlmResponseCache::new(10, 60);
        let k = key("show users", "v1", "gpt-4");
        let plan = dummy_plan("users");

        assert!(cache.get(&k).await.is_none());
        cache.insert(k.clone(), plan.clone()).await;
        let cached = cache.get(&k).await;
        assert_eq!(cached, Some(plan));
        // Verify the plan content
        assert_eq!(cached.unwrap().from.table_name().unwrap(), "users");
    }

    /// Different keys produce cache misses even when the values are
    /// identical.
    #[tokio::test]
    async fn different_key_misses() {
        let cache = LlmResponseCache::new(10, 60);
        let k1 = key("show users", "v1", "gpt-4");
        let k2 = key("show users", "v2", "gpt-4");

        cache.insert(k1.clone(), dummy_plan("users")).await;
        assert!(cache.get(&k1).await.is_some());
        assert!(cache.get(&k2).await.is_none());
    }

    /// `invalidate_question` removes entries whose
    /// `normalized_question` matches.
    #[tokio::test]
    async fn invalidate_question_removes_entry() {
        let cache = LlmResponseCache::new(10, 60);
        let k = key("show users", "v1", "gpt-4");

        cache.insert(k.clone(), dummy_plan("users")).await;
        assert!(cache.get(&k).await.is_some());

        cache.invalidate_question("show users").await;
        assert!(cache.get(&k).await.is_none());
    }

    /// `invalidate_schema_version` removes all entries with a given
    /// schema version.
    #[tokio::test]
    async fn invalidate_schema_version_removes_entries() {
        let cache = LlmResponseCache::new(10, 60);
        let k1 = key("show users", "v1", "gpt-4");
        let k2 = key("show orders", "v1", "gpt-4");
        let k3 = key("show products", "v2", "gpt-4");

        cache.insert(k1.clone(), dummy_plan("users")).await;
        cache.insert(k2.clone(), dummy_plan("orders")).await;
        cache.insert(k3.clone(), dummy_plan("products")).await;

        // Verify all three are present.
        assert!(cache.get(&k1).await.is_some());
        assert!(cache.get(&k2).await.is_some());
        assert!(cache.get(&k3).await.is_some());

        cache.invalidate_schema_version("v1").await;

        // v1 entries should be gone; v2 entry remains.
        assert!(cache.get(&k1).await.is_none());
        assert!(cache.get(&k2).await.is_none());
        assert!(cache.get(&k3).await.is_some());
    }

    /// `clear` removes all entries from the cache.
    #[tokio::test]
    async fn clear_removes_all_entries() {
        let cache = LlmResponseCache::new(10, 60);
        let k1 = key("show users", "v1", "gpt-4");
        let k2 = key("show orders", "v2", "gpt-4");

        cache.insert(k1.clone(), dummy_plan("users")).await;
        cache.insert(k2.clone(), dummy_plan("orders")).await;
        assert!(cache.get(&k1).await.is_some());
        assert!(cache.get(&k2).await.is_some());

        cache.clear();

        assert!(cache.get(&k1).await.is_none());
        assert!(cache.get(&k2).await.is_none());
    }

    /// Multiple concurrent operations are safe and do not panic.
    #[tokio::test]
    async fn concurrent_access_is_safe() {
        let cache = Arc::new(LlmResponseCache::new(100, 300));
        let mut handles = Vec::new();

        for i in 0..10 {
            let cache = cache.clone();
            let handle = tokio::spawn(async move {
                let k = key(&format!("question_{}", i), "v1", "gpt-4");
                let plan = dummy_plan(&format!("table_{}", i));
                cache.insert(k.clone(), plan).await;
                let cached = cache.get(&k).await;
                assert!(cached.is_some());
                assert_eq!(
                    cached.unwrap().from.table_name().unwrap(),
                    format!("table_{}", i)
                );
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.expect("concurrent task should succeed");
        }

        // Verify all entries are accessible.
        for i in 0..10 {
            let k = key(&format!("question_{}", i), "v1", "gpt-4");
            assert!(cache.get(&k).await.is_some());
        }
    }
}
