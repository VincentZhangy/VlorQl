//! Cache for compiled SQL results (`ValidatedPlan → CompiledQuery`).
//!
//! [`CompileCache`] avoids re-compiling a plan that has already been
//! compiled for the same dialect.  The cache uses a [`CompileCacheKey`]
//! that captures the plan's content hash, the SQL dialect, and the
//! identifier quoting style, so that different dialects produce
//! different cache entries.

use crate::cache::CompileCacheKey;
use crate::compile::CompiledQuery;
use crate::schema::DialectProfile;
use crate::validate::ValidatedPlan;
use moka::future::Cache as MokaCache;
use std::sync::Arc;

/// A cache for [`CompiledQuery`] values keyed by plan hash + dialect.
///
/// Uses a [`moka::future::Cache`] with a weigher that limits the total
/// SQL string length, preventing unbounded memory growth.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::CompileCache;
/// use vlorql_core::schema::{DialectProfile, SqlDialect, QueryPlan, Projection, FromClause};
/// use vlorql_core::compile::CompiledQuery;
/// use vlorql_core::validate::ValidatedPlan;
/// use std::sync::Arc;
///
/// # async fn example() {
/// let cache = CompileCache::new(1024, 60);
/// let plan = ValidatedPlan(Arc::new(QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None,
/// }));
/// let profile = DialectProfile::default();
///
/// assert!(cache.get(&plan, &profile).await.is_none());
///
/// let compiled = CompiledQuery {
///     sql: "SELECT \"id\" FROM \"users\"".to_owned(),
///     parameters: vec![],
///     dialect: SqlDialect::Postgres,
/// };
/// cache.insert(&plan, &profile, compiled.clone()).await;
///
/// let cached = cache.get(&plan, &profile).await;
/// assert!(cached.is_some());
/// assert_eq!(cached.unwrap().sql, "SELECT \"id\" FROM \"users\"");
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct CompileCache {
    inner: MokaCache<CompileCacheKey, Arc<CompiledQuery>>,
    max_size: u64,
}

impl CompileCache {
    /// Creates a new compile cache.
    ///
    /// * `max_size` — maximum total weight (sum of SQL string lengths in
    ///   bytes) before the cache evicts least-recently-used items.
    /// * `ttl_seconds` — time-to-live in seconds.  Entries older than
    ///   this are automatically invalidated.
    #[must_use]
    pub fn new(max_size: u64, ttl_seconds: u64) -> Self {
        let mut builder = MokaCache::builder()
            .weigher(|_k, v: &Arc<CompiledQuery>| -> u32 {
                // Weight is the SQL string length in bytes, capped at
                // u32::MAX to stay within moka's range.
                let len = v.sql.len() as u64;
                len.min(u64::from(u32::MAX)) as u32
            })
            .max_capacity(max_size);
        if ttl_seconds > 0 {
            builder = builder.time_to_live(std::time::Duration::from_secs(ttl_seconds));
        }
        let inner = builder.build();
        Self { inner, max_size }
    }

    /// Returns the cached compiled query for `plan` under `profile`, or
    /// `None` on a cache miss.
    ///
    /// The key is computed from the plan's normalised JSON hash, the
    /// dialect, and the quoting style, so the same plan compiled for
    /// different dialects produces different cache entries.
    pub async fn get(
        &self,
        plan: &ValidatedPlan,
        profile: &DialectProfile,
    ) -> Option<Arc<CompiledQuery>> {
        let key = CompileCacheKey::new(plan, profile);
        let result = self.inner.get(&key).await;
        let hit = result.is_some();
        tracing::debug!(
            target: "vlorql::cache",
            "Compile cache {} for plan_hash={:016x}, dialect={:?}",
            if hit { "HIT" } else { "MISS" },
            key.plan_hash,
            key.dialect,
        );
        result
    }

    /// Inserts a compiled query into the cache.
    ///
    /// The key is computed from `plan` and `profile` identically to
    /// [`Self::get`], ensuring that a subsequent `get` with the same
    /// arguments finds the entry.
    pub async fn insert(
        &self,
        plan: &ValidatedPlan,
        profile: &DialectProfile,
        query: CompiledQuery,
    ) {
        let key = CompileCacheKey::new(plan, profile);
        tracing::debug!(
            target: "vlorql::cache",
            "Compile cache INSERT for plan_hash={:016x}, dialect={:?}",
            key.plan_hash,
            key.dialect,
        );
        self.inner.insert(key, Arc::new(query)).await;
    }

    /// Removes the cache entry for `plan` under `profile`.
    pub async fn invalidate_plan(&self, plan: &ValidatedPlan, profile: &DialectProfile) {
        let key = CompileCacheKey::new(plan, profile);
        self.inner.invalidate(&key).await;
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

    /// Returns the maximum total weight of the cache.
    #[must_use]
    pub fn max_size(&self) -> u64 {
        self.max_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        FromClause, Projection, QueryPlan,
        SqlDialect,
    };
    use std::sync::Arc;

    fn make_plan() -> ValidatedPlan {
        ValidatedPlan(Arc::new(QueryPlan {
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
        }))
    }

    fn make_compiled(dialect: SqlDialect) -> CompiledQuery {
        match dialect {
            SqlDialect::Postgres => CompiledQuery {
                sql: "SELECT \"id\" FROM \"users\"".to_owned(),
                parameters: vec![],
                dialect: SqlDialect::Postgres,
            },
            SqlDialect::Sqlite => CompiledQuery {
                sql: "SELECT \"id\" FROM \"users\"".to_owned(),
                parameters: vec![],
                dialect: SqlDialect::Sqlite,
            },
            SqlDialect::MySql => CompiledQuery {
                sql: "SELECT `id` FROM `users`".to_owned(),
                parameters: vec![],
                dialect: SqlDialect::MySql,
            },
        }
    }

    /// Same plan + same profile → cache hit.
    #[tokio::test]
    async fn same_plan_same_dialect_hits() {
        let cache = CompileCache::new(1024, 60);
        let plan = make_plan();
        let profile = DialectProfile::default();
        let compiled = make_compiled(SqlDialect::Postgres);

        assert!(cache.get(&plan, &profile).await.is_none());
        cache.insert(&plan, &profile, compiled.clone()).await;
        let cached = cache.get(&plan, &profile).await;
        assert_eq!(cached, Some(Arc::new(compiled)));
    }

    /// Same plan + different dialect → different cache entries.
    #[tokio::test]
    async fn same_plan_different_dialect_misses() {
        let cache = CompileCache::new(1024, 60);
        let plan = make_plan();

        let pg_profile = DialectProfile::default();
        let sqlite_profile = DialectProfile {
            dialect: SqlDialect::Sqlite,
            ..DialectProfile::default()
        };

        let pg_compiled = make_compiled(SqlDialect::Postgres);
        cache
            .insert(&plan, &pg_profile, pg_compiled)
            .await;

        // SQLite query should not be found because the key includes
        // the dialect — even though the plan is the same.
        assert!(
            cache.get(&plan, &sqlite_profile).await.is_none(),
            "a different dialect must not return the cached value"
        );
    }

    /// Different plans produce different keys even with the same dialect.
    #[tokio::test]
    async fn different_plan_different_key() {
        let cache = CompileCache::new(1024, 60);
        let profile = DialectProfile::default();

        let plan_a = make_plan();
        let plan_b = ValidatedPlan(Arc::new(QueryPlan {
            select: vec![Projection::Column {
                table: None,
                column: "email".to_owned(),
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
        }));

        let compiled_a = make_compiled(SqlDialect::Postgres);
        let compiled_b = CompiledQuery {
            sql: "SELECT \"email\" FROM \"users\"".to_owned(),
            parameters: vec![],
            dialect: SqlDialect::Postgres,
        };

        cache.insert(&plan_a, &profile, compiled_a).await;
        cache.insert(&plan_b, &profile, compiled_b.clone()).await;

        // plan_b should find its own entry.
        let cached = cache.get(&plan_b, &profile).await;
        assert_eq!(cached, Some(Arc::new(compiled_b)));
    }

    /// Cache respects the max_size weigher (small size for testing).
    #[tokio::test]
    async fn cache_evicts_under_lru_weight_limit() {
        // Use a very small max_size so entries are evicted quickly.
        let cache = CompileCache::new(50, 60); // 50 bytes total weight
        let profile = DialectProfile::default();

        // Insert entries until the cache evicts the oldest.
        for i in 0..20 {
            let plan = ValidatedPlan(Arc::new(QueryPlan {
                select: vec![Projection::Column {
                    table: None,
                    column: format!("col_{i}"),
                    alias: None,
                }],
                from: FromClause {
                    table: "t".to_owned(),
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
            }));
            let compiled = CompiledQuery {
                sql: format!("SELECT \"col_{i}\" FROM \"t\""),
                parameters: vec![],
                dialect: SqlDialect::Postgres,
            };
            cache.insert(&plan, &profile, compiled).await;
        }

        // The cache should have evicted some entries.
        // We can't assert the exact count because moka's eviction is
        // asynchronous, but we can verify it's not growing unbounded.
        let size = cache.size();
        assert!(
            size < 20,
            "cache should have evicted entries under tight weight limit, got {size}"
        );
    }

    /// Invalidate a specific plan.
    #[tokio::test]
    async fn invalidate_plan_removes_entry() {
        let cache = CompileCache::new(1024, 60);
        let plan = make_plan();
        let profile = DialectProfile::default();
        let compiled = make_compiled(SqlDialect::Postgres);

        cache.insert(&plan, &profile, compiled).await;
        assert!(cache.get(&plan, &profile).await.is_some());

        cache.invalidate_plan(&plan, &profile).await;
        assert!(
            cache.get(&plan, &profile).await.is_none(),
            "entry should be removed after invalidation"
        );
    }

    /// Clear removes all entries.
    #[tokio::test]
    async fn clear_removes_all_entries() {
        let cache = CompileCache::new(1024, 60);
        let plan = make_plan();
        let profile = DialectProfile::default();
        let compiled = make_compiled(SqlDialect::Postgres);

        cache.insert(&plan, &profile, compiled).await;
        assert!(cache.get(&plan, &profile).await.is_some());

        cache.clear();
        assert!(
            cache.get(&plan, &profile).await.is_none(),
            "entry should be removed after clear"
        );
    }

    /// TTL expiry causes a cache miss.
    #[tokio::test]
    async fn ttl_expiry_causes_miss() {
        let cache = CompileCache::new(1024, 1); // 1 second TTL
        let plan = make_plan();
        let profile = DialectProfile::default();
        let compiled = make_compiled(SqlDialect::Postgres);

        cache.insert(&plan, &profile, compiled).await;
        assert!(cache.get(&plan, &profile).await.is_some());

        // Wait for the entry to expire.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        assert!(
            cache.get(&plan, &profile).await.is_none(),
            "entry should have expired after TTL"
        );
    }

    /// Concurrent access to the cache should not cause data races.
    #[tokio::test]
    async fn concurrent_access_is_safe() {
        let cache = std::sync::Arc::new(CompileCache::new(1024, 60));
        let profile = DialectProfile::default();
        let mut handles = Vec::new();

        for i in 0..20 {
            let cache = Arc::clone(&cache);
            let profile = profile.clone();
            handles.push(tokio::spawn(async move {
                let plan = ValidatedPlan(Arc::new(QueryPlan {
                    select: vec![Projection::Column {
                        table: None,
                        column: format!("col_{i}"),
                        alias: None,
                    }],
                    from: FromClause {
                        table: "t".to_owned(),
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
                }));
                let compiled = CompiledQuery {
                    sql: format!("SELECT \"col_{i}\" FROM \"t\""),
                    parameters: vec![],
                    dialect: SqlDialect::Postgres,
                };

                // Insert and immediately retrieve.
                cache.insert(&plan, &profile, compiled).await;
                let result = cache.get(&plan, &profile).await;
                assert!(result.is_some(), "concurrent insert+get should succeed");
            }));
        }

        for handle in handles {
            handle.await.expect("concurrent task should not panic");
        }
    }
}