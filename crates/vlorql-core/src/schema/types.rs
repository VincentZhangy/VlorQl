//! Primitive types used by query plans and schema metadata.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A SQL-compatible type understood by the query planner.
///
/// The variants map to a small set of storage types common across the
/// supported dialects. The validator and the SQL compiler use this
/// enum to check expression types and to pick the right literal
/// serialization for bind parameters.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::DataType;
///
/// let int_type = DataType::Int;
/// let str_type = DataType::String;
/// assert_ne!(int_type, str_type);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// 64-bit signed integer.
    Int,
    /// IEEE-754 double-precision float.
    Float,
    /// Variable-length UTF-8 text.
    String,
    /// Boolean (`true` / `false`).
    Boolean,
    /// Calendar date without a time zone.
    Date,
    /// Timestamp with microsecond precision.
    Timestamp,
    /// Untyped JSON value.
    Json,
    /// SQL `NULL` of indeterminate type.
    Null,
    /// Universally unique identifier.
    Uuid,
}

/// Operators that combine two expressions or values.
///
/// Used inside [`Expression::BinaryOp`](crate::schema::Expression::BinaryOp).
/// `Eq` / `Neq` / `Gt` / `Lt` / `Gte` / `Lte` / `Like` / `ILike` are
/// all valid in a `WHERE` clause; the arithmetic and boolean variants
/// are mostly used in projections and join predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BinaryOperator {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// Boolean AND.
    And,
    /// Boolean OR.
    Or,
    /// `=`.
    Eq,
    /// `<>`.
    Neq,
    /// `>`.
    Gt,
    /// `<`.
    Lt,
    /// `>=`.
    Gte,
    /// `<=`.
    Lte,
    /// `LIKE` (case-sensitive pattern match).
    Like,
    /// `ILIKE` (PostgreSQL case-insensitive pattern match).
    ILike,
}

/// Operators used by a comparison predicate.
///
/// `In` and `Between` are not rendered by the SQL compiler directly;
/// they must be used inside the corresponding [`Predicate`](crate::schema::Predicate)
/// variants (`In`, `Between`) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonOperator {
    /// `=`.
    Eq,
    /// `<>`.
    Neq,
    /// `>`.
    Gt,
    /// `<`.
    Lt,
    /// `>=`.
    Gte,
    /// `<=`.
    Lte,
    /// Case-sensitive pattern match.
    Like,
    /// Case-insensitive pattern match (PostgreSQL only).
    ILike,
    /// List membership; requires the [`Predicate::In`](crate::schema::Predicate::In) variant.
    In,
    /// Range check; requires the [`Predicate::Between`](crate::schema::Predicate::Between) variant.
    Between,
}

/// Supported SQL join types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinType {
    /// `INNER JOIN` — keep only rows that match on both sides.
    Inner,
    /// `LEFT JOIN` — keep all rows from the left side, fill with `NULL` on the right.
    Left,
    /// `RIGHT JOIN` — keep all rows from the right side, fill with `NULL` on the left.
    Right,
    /// `FULL OUTER JOIN` — keep all rows from both sides.
    Full,
    /// `CROSS JOIN` — Cartesian product.
    Cross,
}

/// SQL dialects supported by the initial compiler set.
///
/// Each variant has a dedicated [`SqlCompiler`](crate::compile::SqlCompiler)
/// implementation that knows how to emit dialect-specific syntax
/// (placeholders, pagination, identifier quoting).
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::SqlDialect;
///
/// let dialect = SqlDialect::Postgres;
/// assert_eq!(format!("{dialect:?}"), "Postgres");
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SqlDialect {
    /// PostgreSQL 12+ (`$1` placeholders, double-quoted identifiers, `LIMIT n OFFSET m`).
    #[default]
    Postgres,
    /// SQLite 3 (`?` placeholders, double-quoted identifiers, `LIMIT n OFFSET m`).
    Sqlite,
    /// MySQL 8 (`?` placeholders, backtick-quoted identifiers, `LIMIT offset, limit`).
    MySql,
}

/// How identifiers should be quoted when SQL is generated.
///
/// The default value, [`IdentifierQuoting::DoubleQuote`], is correct
/// for PostgreSQL, SQLite, and most ANSI-compliant dialects.
/// [`IdentifierQuoting::Backtick`] is required for MySQL.
/// [`IdentifierQuoting::Never`] skips quoting entirely and is only
/// safe with trusted identifiers; [`IdentifierQuoting::Always`] is
/// an internal sentinel that the compiler resolves to a dialect
/// default rather than producing SQL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdentifierQuoting {
    /// Skip identifier quoting; the identifier must already be a
    /// valid unquoted SQL identifier.
    Never,
    /// Sentinel: resolve to the dialect default
    /// (PostgreSQL/SQLite -> `DoubleQuote`, MySQL -> `Backtick`).
    Always,
    /// ANSI-style `"identifier"` quoting.
    #[default]
    DoubleQuote,
    /// MySQL-style `` `identifier` `` quoting.
    Backtick,
}
