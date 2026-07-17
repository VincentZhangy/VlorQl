//! Tagged expressions and predicates used in query plans.
//!
//! Both [`Expression`] and [`Predicate`] are serialized as
//! internally-tagged JSON enums (`{"type": "...", ...}`). The
//! validators and the SQL compiler inspect the tag to decide how to
//! type-check or render the node.

use super::types::{BinaryOperator, ComparisonOperator, DataType};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An expression that can be selected, filtered, grouped, or ordered.
///
/// The enum is serialized with `#[serde(tag = "type", rename_all = "snake_case")]`,
/// so a literal payload looks like
/// `{"type": "literal", "value": 42, "data_type": "int"}` and a column
/// reference looks like
/// `{"type": "column_ref", "table": "users", "column": "id"}`.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{Expression, DataType};
///
/// let lit = Expression::Literal {
///     value: serde_json::json!(42),
///     data_type: DataType::Int,
/// };
/// let col = Expression::ColumnRef {
///     table: Some("users".to_owned()),
///     column: "id".to_owned(),
/// };
/// assert_ne!(lit, col);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expression {
    /// A literal value together with its SQL type.
    Literal {
        /// The literal value as a JSON-compatible scalar (string,
        /// number, boolean, or `null`).
        value: serde_json::Value,
        /// The declared SQL type of the literal.
        data_type: DataType,
    },
    /// A reference to a column, optionally qualified by a table.
    ColumnRef {
        /// Source-table qualifier (may be `None` for an unqualified
        /// reference, in which case the validator resolves it
        /// against the plan's `from` and joined tables).
        table: Option<String>,
        /// The column name as it appears in the schema.
        column: String,
    },
    /// A scalar or aggregate function call.
    FunctionCall {
        /// Function name as emitted to the SQL backend (case is
        /// preserved).
        name: String,
        /// Arguments passed positionally to the function.
        args: Vec<Expression>,
        /// When `true`, the call is rendered as `DISTINCT` (subject
        /// to the dialect profile's `allow_distinct` setting).
        distinct: bool,
    },
    /// A binary operation between two expressions.
    BinaryOp {
        /// Left-hand operand.
        left: Box<Expression>,
        /// The operator to apply.
        op: BinaryOperator,
        /// Right-hand operand.
        right: Box<Expression>,
    },
}

/// A boolean condition used by `WHERE`, `HAVING`, and join clauses.
///
/// Like [`Expression`], this is serialized with an internal `type`
/// tag. The `In` and `Between` variants are the only way to express
/// list membership and range checks; the bare comparison operators
/// of the same name are reserved for the dedicated predicate shapes.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{Predicate, Expression, ComparisonOperator, DataType};
///
/// let pred = Predicate::Comparison {
///     left: Expression::ColumnRef {
///         table: Some("users".to_owned()),
///         column: "age".to_owned(),
///     },
///     op: ComparisonOperator::Gte,
///     right: Expression::Literal {
///         value: serde_json::json!(18),
///         data_type: DataType::Int,
///     },
/// };
/// assert!(matches!(pred, Predicate::Comparison { .. }));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    /// A comparison between two expressions.
    Comparison {
        /// Left-hand operand.
        left: Expression,
        /// The comparison operator.
        op: ComparisonOperator,
        /// Right-hand operand.
        right: Expression,
    },
    /// The conjunction of two predicates.
    And {
        /// Left sub-predicate.
        left: Box<Predicate>,
        /// Right sub-predicate.
        right: Box<Predicate>,
    },
    /// The disjunction of two predicates.
    Or {
        /// Left sub-predicate.
        left: Box<Predicate>,
        /// Right sub-predicate.
        right: Box<Predicate>,
    },
    /// The negation of a predicate.
    Not {
        /// The predicate to negate.
        child: Box<Predicate>,
    },
    /// A value constrained to an inclusive range.
    Between {
        /// The value being tested.
        expr: Expression,
        /// Inclusive lower bound.
        low: Expression,
        /// Inclusive upper bound.
        high: Expression,
    },
    /// A value constrained to a list of expressions.
    In {
        /// The value being tested.
        expr: Expression,
        /// Allowed values.
        values: Vec<Expression>,
    },
    /// A string pattern match.
    Like {
        /// The value being tested.
        expr: Expression,
        /// Pattern string (using `LIKE`/`ILIKE` wildcards).
        pattern: String,
    },
    /// A null check.
    IsNull {
        /// The value being tested.
        expr: Expression,
    },
}
