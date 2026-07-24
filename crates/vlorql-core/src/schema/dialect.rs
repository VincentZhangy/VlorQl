//! Controlled SQL dialect capabilities.

use super::types::{IdentifierQuoting, JoinType, SqlDialect};
use derive_builder::Builder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The SQL capabilities allowed for one query-planning context.
///
/// Each profile captures the dialect family, identifier quoting
/// convention, the set of allowed / denied features, and a few
/// quantitative limits that the validator and SQL compiler consult.
/// Use [`DialectProfile::builder`] to construct a customized profile
/// from the defaults.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{DialectProfile, SqlDialect, JoinType};
///
/// let profile = DialectProfile::builder()
///     .dialect(SqlDialect::Sqlite)
///     .max_joins(5)
///     .build()
///     .expect("valid profile");
/// assert_eq!(profile.dialect, SqlDialect::Sqlite);
/// assert_eq!(profile.max_joins, Some(5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Builder)]
#[builder(setter(into), default)]
#[serde(default, deny_unknown_fields)]
pub struct DialectProfile {
    /// The SQL dialect this profile emits.
    pub dialect: SqlDialect,
    /// How identifiers should be quoted in the generated SQL.
    pub quote_style: IdentifierQuoting,
    /// Whether `WITH <cte> ... SELECT ...` is allowed.
    pub supports_cte: bool,
    /// Whether window functions are allowed.
    pub supports_window_functions: bool,
    /// Whether JSON operators (`->`, `->>`, `@@`, …) are allowed.
    pub supports_json_operations: bool,
    /// Maximum number of joins permitted in a single plan. `None`
    /// means no limit.
    pub max_joins: Option<usize>,
    /// The join types the validator allows. An empty list means
    /// "all join types allowed".
    pub allowed_join_types: Vec<JoinType>,
    /// The function names the validator allows. An empty list
    /// means "any function not explicitly denied".
    pub allowed_functions: Vec<String>,
    /// The function names the validator always rejects.
    pub denied_functions: Vec<String>,
    /// Maximum number of `GROUP BY` expressions. `None` means no
    /// limit.
    pub max_group_by_columns: Option<usize>,
    /// Whether `DISTINCT` is permitted inside function calls.
    pub allow_distinct: bool,
    /// Whether `SELECT DISTINCT` (whole-query dedup) is allowed.
    pub allow_select_distinct: bool,
    /// Whether `OFFSET n` is allowed.
    pub supports_offset: bool,
    /// Whether `FETCH FIRST n ROWS ONLY` is allowed.
    pub supports_fetch: bool,
}

impl Default for DialectProfile {
    fn default() -> Self {
        Self {
            dialect: SqlDialect::Postgres,
            quote_style: IdentifierQuoting::DoubleQuote,
            supports_cte: true,
            supports_window_functions: true,
            supports_json_operations: true,
            max_joins: None,
            allowed_join_types: vec![
                JoinType::Inner,
                JoinType::Left,
                JoinType::Right,
                JoinType::Full,
                JoinType::Cross,
            ],
            allowed_functions: Vec::new(),
            denied_functions: Vec::new(),
            max_group_by_columns: None,
            allow_distinct: true,
            allow_select_distinct: true,
            supports_offset: true,
            supports_fetch: true,
        }
    }
}

impl DialectProfile {
    /// Returns a builder initialized with the profile defaults.
    pub fn builder() -> DialectProfileBuilder {
        DialectProfileBuilder::default()
    }
}
