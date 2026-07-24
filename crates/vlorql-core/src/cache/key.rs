//! Cache key types for identifying cached artifacts.
//!
//! [`SchemaCacheKey`] identifies a versioned schema snapshot;
//! [`CompileCacheKey`] identifies a compiled SQL result by plan hash,
//! dialect, and quoting style.

use crate::schema::{DialectProfile, IdentifierQuoting, SqlDialect};
use crate::validate::ValidatedPlan;
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

/// Key used to cache schema snapshots by version and source.
///
/// Two snapshots with the same version and source are assumed to be
/// identical, so the cache can return a cached snapshot without
/// re-parsing or re-fetching.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchemaCacheKey {
    /// User-specified version string, e.g. `"v1.2.3"`.
    pub version: String,
    /// Data-source identifier, e.g. `"postgres://prod-db.example.com:5432"`.
    pub source: String,
}

/// Key used to cache compiled SQL results.
///
/// The key is derived from the [`QueryPlan`](crate::schema::QueryPlan)
/// content (via a deterministic hash of its normalised JSON form) and
/// the dialect/quoting parameters that affect the generated SQL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompileCacheKey {
    /// 64-bit hash of the normalised query plan.
    pub plan_hash: u64,
    /// Target SQL dialect.
    pub dialect: SqlDialect,
    /// Identifier quoting style.
    pub quote_style: IdentifierQuoting,
}

impl CompileCacheKey {
    /// Creates a new key from a validated plan and a dialect profile.
    ///
    /// The plan is normalised (serialised to JSON with sorted keys,
    /// `None` values skipped) before hashing, so two semantically
    /// equivalent plans always produce the same key.
    ///
    /// # Examples
    ///
    /// ```
    /// use vlorql_core::cache::CompileCacheKey;
    /// use vlorql_core::schema::{DialectProfile, QueryPlan, Projection, FromClause};
    /// use vlorql_core::validate::ValidatedPlan;
    /// use std::sync::Arc;
    ///
    /// let plan = QueryPlan {
    ///     select: vec![Projection::Column {
    ///         table: None, column: "id".to_owned(), alias: None,
    ///     }],
    ///     from: FromClause { table: "users".to_owned(), alias: None },
    ///     r#where: None, group_by: None, having: None,
    ///     order_by: None, limit: None, offset: None,
    ///     joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    /// };
    /// let validated = ValidatedPlan(Arc::new(plan));
    /// let profile = DialectProfile::default();
    /// let key = CompileCacheKey::new(&validated, &profile);
    /// assert!(key.plan_hash != 0);
    /// ```
    pub fn new(plan: &ValidatedPlan, profile: &DialectProfile) -> Self {
        let normalized = super::normalize::normalize_plan(plan);
        let hash = xxh3_64(normalized.as_bytes());
        Self {
            plan_hash: hash,
            dialect: profile.dialect,
            quote_style: profile.quote_style,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FromClause, Projection, QueryPlan};
    use std::sync::Arc;

    fn make_validated_plan() -> ValidatedPlan {
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
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }))
    }

    /// The same plan + profile always produces the same key.
    #[test]
    fn same_plan_same_key() {
        let plan = make_validated_plan();
        let profile = DialectProfile::default();
        let a = CompileCacheKey::new(&plan, &profile);
        let b = CompileCacheKey::new(&plan, &profile);
        assert_eq!(a, b);
    }

    /// Different plans produce different keys.
    #[test]
    fn different_plan_different_key() {
        let profile = DialectProfile::default();

        let plan_a = make_validated_plan();

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
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }));

        let key_a = CompileCacheKey::new(&plan_a, &profile);
        let key_b = CompileCacheKey::new(&plan_b, &profile);
        assert_ne!(key_a, key_b);
    }

    /// Different dialect profiles produce different keys.
    #[test]
    fn different_dialect_different_key() {
        let plan = make_validated_plan();

        let pg = DialectProfile::default();
        let sqlite = DialectProfile {
            dialect: SqlDialect::Sqlite,
            ..DialectProfile::default()
        };

        let key_pg = CompileCacheKey::new(&plan, &pg);
        let key_sqlite = CompileCacheKey::new(&plan, &sqlite);
        assert_ne!(key_pg, key_sqlite);
    }

    /// SchemaCacheKey equality works correctly.
    #[test]
    fn schema_cache_key_equality() {
        let a = SchemaCacheKey {
            version: "v1".to_owned(),
            source: "db://prod".to_owned(),
        };
        let b = SchemaCacheKey {
            version: "v1".to_owned(),
            source: "db://prod".to_owned(),
        };
        let c = SchemaCacheKey {
            version: "v2".to_owned(),
            source: "db://prod".to_owned(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
