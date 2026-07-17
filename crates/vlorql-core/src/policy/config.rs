//! Policy configuration models.

use crate::schema::Predicate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Access-control policies applied to query plans.
///
/// `PolicyConfig` is a free-form bag of rules: per-table policies,
/// a list of globally denied columns, and a list of row filters that
/// must be combined with the plan's `WHERE` clause. The
/// [`policy::PolicyEngine`](crate::policy::PolicyEngine) consumes a
/// config to validate plans and to derive mandatory row filters.
///
/// # Examples
///
/// ```
/// use vlorql_core::policy::PolicyConfig;
///
/// let config = PolicyConfig::default();
/// assert!(config.table_policies.is_empty());
/// assert!(config.global_denied_columns.is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Per-table access and column policies, keyed by schema table name.
    pub table_policies: HashMap<String, TablePolicy>,
    /// Column names denied on every table.
    pub global_denied_columns: Vec<String>,
    /// Additional row-level filters matched to the tables they reference.
    pub row_filters: Vec<RowFilter>,
}

/// Access-control settings for one table.
///
/// The default policy allows access to the table and every visible
/// schema column. Operators tighten it by listing columns in
/// `allowed_columns` (positive allowlist), `denied_columns`
/// (negative denylist), or by attaching a `row_filter` that the
/// engine will splice into every `WHERE` clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TablePolicy {
    /// Whether the table may be referenced at all.
    pub allowed: bool,
    /// When present, the only columns that may be referenced.
    pub allowed_columns: Option<Vec<String>>,
    /// Columns denied even when they are included in `allowed_columns`.
    pub denied_columns: Vec<String>,
    /// A mandatory condition to append when this table is queried.
    pub row_filter: Option<RowFilter>,
}

impl Default for TablePolicy {
    fn default() -> Self {
        Self {
            allowed: true,
            allowed_columns: None,
            denied_columns: Vec::new(),
            row_filter: None,
        }
    }
}

/// A mandatory row-level predicate and its human-readable purpose.
///
/// The engine combines every `RowFilter` whose table scope matches
/// the plan into a single `AND` tree via
/// [`PolicyEngine::apply_row_filters`](crate::policy::PolicyEngine::apply_row_filters).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RowFilter {
    /// The boolean condition that must hold for every returned row.
    pub condition: Predicate,
    /// Short human-readable description of the filter's purpose
    /// (e.g. `"tenant isolation"`).
    pub description: String,
}
