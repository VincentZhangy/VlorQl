//! Core data types for the function registry.
//!
//! Defines [`FunctionKind`], [`Dialect`], and [`FunctionDef`] – the
//! building blocks used by the builder, registry, and all downstream
//! consumers.

use std::borrow::Cow;

use crate::schema::DataType;

/// The broad category of a SQL function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    /// Scalar function (e.g. `UPPER`, `LENGTH`).
    Scalar,
    /// Aggregate function (e.g. `SUM`, `COUNT`).
    Aggregate,
    /// Window function (e.g. `ROW_NUMBER`).
    Window,
}

/// Target SQL dialect for function availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// Available in all dialects (generic).
    Generic,
    /// PostgreSQL.
    Postgres,
    /// MySQL.
    MySql,
    /// SQLite.
    Sqlite,
}

/// Metadata for a SQL function.
#[derive(Debug, Clone)]
pub struct FunctionDef {
    /// All accepted names (first is the canonical name).
    pub names: Vec<Cow<'static, str>>,
    /// The category of the function.
    pub kind: FunctionKind,
    /// Minimum number of arguments (inclusive).
    pub min_args: usize,
    /// Maximum number of arguments (`None` = unlimited).
    pub max_args: Option<usize>,
    /// Expected parameter types for type-checking (`None` = skip check).
    pub param_types: Option<Vec<Option<DataType>>>,
    /// Return type for type inference (`None` = unknown).
    pub return_type: Option<DataType>,
    /// Whether `DISTINCT` is allowed (e.g. `COUNT(DISTINCT col)`).
    pub supports_distinct: bool,
    /// Whether an `ORDER BY` child clause is allowed.
    pub supports_order_by: bool,
    /// Whether `*` is accepted as an argument (e.g. `COUNT(*)`).
    pub allows_star: bool,
    /// Dialects this function is available in. Empty = all dialects.
    pub dialects: Vec<Dialect>,
}

impl FunctionDef {
    /// Returns the canonical (first) name.
    pub fn canonical_name(&self) -> &str {
        self.names.first().map_or("", |n| n.as_ref())
    }

    /// Returns `true` if `name` matches the canonical name or any alias.
    pub fn accepts_name(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// Returns `true` if this function is available for the given dialect.
    pub fn supports_dialect(&self, dialect: Dialect) -> bool {
        self.dialects.is_empty() || self.dialects.contains(&Dialect::Generic) || self.dialects.contains(&dialect)
    }
}