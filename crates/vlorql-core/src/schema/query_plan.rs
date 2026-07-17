//! The structured query plan emitted by an LLM.

use super::expressions::{Expression, Predicate};
use super::types::JoinType;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A projection in the `SELECT` list.
///
/// Every variant is rendered as a single column expression in the
/// compiled SQL, optionally paired with an `AS <alias>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Projection {
    /// Select one named column.
    Column {
        /// Source-table qualifier, if the column reference should be
        /// qualified in the rendered SQL.
        table: Option<String>,
        /// The column name as it appears in the schema.
        column: String,
        /// Optional alias (`AS <alias>`).
        alias: Option<String>,
    },
    /// Select a computed expression.
    Expr {
        /// The expression to render.
        expression: Expression,
        /// Optional alias (`AS <alias>`).
        alias: Option<String>,
    },
    /// Select all columns from an optional table qualifier.
    Star {
        /// When `Some(table)`, render as `<table>.*`; when `None`,
        /// render as a bare `*`.
        table: Option<String>,
    },
}

/// The source table for a query or join.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FromClause {
    /// The table name as it appears in the schema snapshot.
    pub table: String,
    /// Optional alias (`AS <alias>`).
    pub alias: Option<String>,
}

/// A join between the current relation and another table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinClause {
    /// The kind of join to emit.
    pub join_type: JoinType,
    /// The right-hand side of the join (table + optional alias).
    pub right_table: FromClause,
    /// The boolean expression that decides which rows match.
    /// `CROSS JOIN` is the only join type that ignores this field.
    pub on: Predicate,
}

/// An expression and its requested sort direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrderByTerm {
    /// The expression whose value determines the ordering.
    pub expr: Expression,
    /// `true` to sort in descending order, `false` for ascending.
    pub descending: bool,
}

/// A named common table expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommonTableExpression {
    /// The CTE name exposed to the outer query.
    pub name: String,
    /// The query that produces the CTE's rows.
    pub query: Box<QueryPlan>,
}

/// A complete structured query plan.
///
/// The plan is what the LLM is expected to emit (as JSON) and what
/// the validator and SQL compiler consume. The serde annotations
/// turn the plan into a self-describing JSON object with
/// `snake_case` field names, omit `None` pagination values, and
/// reject unknown keys (so the LLM cannot smuggle in extra fields).
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause, Expression, Predicate, DataType, ComparisonOperator};
///
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: Some("users".to_owned()),
///         column: "id".to_owned(),
///         alias: None,
///     }],
///     from: FromClause { table: "users".to_owned(), alias: None },
///     r#where: Some(Predicate::Comparison {
///         left: Expression::ColumnRef {
///             table: Some("users".to_owned()),
///             column: "id".to_owned(),
///         },
///         op: ComparisonOperator::Gt,
///         right: Expression::Literal {
///             value: serde_json::json!(10),
///             data_type: DataType::Int,
///         },
///     }),
///     group_by: None,
///     having: None,
///     order_by: None,
///     limit: None,
///     offset: None,
///     joins: None,
///     ctes: None,
/// };
/// assert_eq!(plan.select.len(), 1);
/// assert!(plan.r#where.is_some());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryPlan {
    /// The projection list.
    pub select: Vec<Projection>,
    /// The primary source table.
    pub from: FromClause,
    /// Optional `WHERE` predicate.
    #[serde(default)]
    pub r#where: Option<Predicate>,
    /// Optional `GROUP BY` expressions.
    #[serde(default)]
    pub group_by: Option<Vec<Expression>>,
    /// Optional `HAVING` predicate.
    #[serde(default)]
    pub having: Option<Predicate>,
    /// Optional `ORDER BY` terms.
    #[serde(default)]
    pub order_by: Option<Vec<OrderByTerm>>,
    /// Optional `LIMIT` clause (the maximum number of rows to return).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Optional `OFFSET` clause (the number of rows to skip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Optional list of joins applied after `from`.
    #[serde(default)]
    pub joins: Option<Vec<JoinClause>>,
    /// Optional list of `WITH` CTEs.
    #[serde(default)]
    pub ctes: Option<Vec<CommonTableExpression>>,
}
