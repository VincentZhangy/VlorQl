//! Normalisation helpers for deterministic cache-key generation.
//!
//! [`normalize_plan`] serialises a [`ValidatedPlan`] into a canonical
//! JSON string so that two semantically equivalent plans (differing
//! only in field ordering or whitespace) produce the same hash.

use crate::validate::ValidatedPlan;
use serde_json::Value;

/// Serialises a validated plan into a canonical JSON string suitable
/// for hashing.
///
/// The normalisation rules are:
///
/// 1. Serialise the [`QueryPlan`] to a [`serde_json::Value`] (field
///    order follows the struct definition — `serde_json` uses `BTreeMap`
///    internally for `Value::Object`, so keys are already sorted).
/// 2. Recursively sort the keys of every JSON object (defensive —
///    `serde_json` already sorts, but this guards against future
///    changes).
/// 3. Serialise the sorted value to a compact string (no whitespace).
///
/// This ensures two plans that are semantically identical but have
/// fields in different orders produce the same normalised string.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::normalize_plan;
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause};
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
///     joins: None, ctes: None,
/// };
/// let validated = ValidatedPlan(Arc::new(plan));
/// let json = normalize_plan(&validated);
/// assert!(json.contains("users"));
/// assert!(json.contains("id"));
/// ```
pub fn normalize_plan(plan: &ValidatedPlan) -> String {
    // Serialize to a serde_json::Value first.  serde_json internally
    // uses BTreeMap for objects, so keys are already sorted.
    let value = serde_json::to_value(plan.as_plan())
        .expect("ValidatedPlan should always serialize to JSON");

    // Defensively sort keys recursively.
    let sorted = sort_value(value);

    // Compact serialisation — no whitespace.
    serde_json::to_string(&sorted).expect("sorted JSON should serialize")
}

/// Recursively sorts the keys of every JSON object in `value`.
fn sort_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::with_capacity(map.len());
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let val = map.get(&key).expect("key exists");
                sorted.insert(key, sort_value(val.clone()));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FromClause, Projection, QueryPlan};
    use std::sync::Arc;

    /// Two plans with the same data produce the same normalised string.
    #[test]
    fn same_plan_same_normalized() {
        let plan = || -> ValidatedPlan {
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
        };

        assert_eq!(normalize_plan(&plan()), normalize_plan(&plan()));
    }

    /// Different plans produce different normalised strings.
    #[test]
    fn different_plan_different_normalized() {
        let plan_a = ValidatedPlan(Arc::new(QueryPlan {
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
        }));

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

        assert_ne!(normalize_plan(&plan_a), normalize_plan(&plan_b));
    }
}