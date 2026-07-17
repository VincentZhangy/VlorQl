//! Compiler traits and result types.

use crate::errors::VlorQLError;
use crate::schema::{DataType, SqlDialect};
use crate::validate::ValidatedPlan;

/// A SQL statement together with its ordered bind parameters and dialect.
///
/// The `sql` field uses the dialect's placeholder syntax
/// (`$1`/`$2`/… for PostgreSQL, `?` for SQLite and MySQL), and the
/// `parameters` field carries the values that should be bound to
/// those placeholders in textual order. Drivers should never
/// interpolate the literal values into the SQL string.
///
/// # Examples
///
/// ```
/// use vlorql_core::compile::CompiledQuery;
/// use vlorql_core::schema::{DataType, SqlDialect};
///
/// let query = CompiledQuery {
///     sql: "SELECT id FROM users WHERE id > $1".to_owned(),
///     parameters: vec![],
///     dialect: SqlDialect::Postgres,
/// };
/// assert!(query.sql.contains("$1"));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    /// The rendered SQL with dialect-specific placeholders.
    pub sql: String,
    /// Ordered bind values matching the placeholders in `sql`.
    pub parameters: Vec<Parameter>,
    /// The dialect that produced this query.
    pub dialect: SqlDialect,
}

/// One ordered value bound to a generated SQL placeholder.
///
/// # Examples
///
/// ```
/// use vlorql_core::compile::Parameter;
/// use vlorql_core::schema::DataType;
///
/// let param = Parameter {
///     value: serde_json::json!(42),
///     data_type: DataType::Int,
/// };
/// assert_eq!(param.value, 42);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    /// The literal value, in the same JSON-compatible form used by [`serde_json::Value`].
    pub value: serde_json::Value,
    /// The declared SQL type of the value.
    pub data_type: DataType,
}

/// Compiles an already validated query plan into parameterized SQL.
///
/// The trait is implemented by each [`SqlDialect`]. The
/// [`crate::compile::QueryBuilder`] contains the rendering logic
/// shared by all dialects; the trait is the dispatch surface.
///
/// # Examples
///
/// ```
/// use vlorql_core::compile::{SqlCompiler, CompiledQuery, PostgresCompiler};
/// use vlorql_core::errors::VlorQLError;
/// use vlorql_core::schema::{QueryPlan, Projection, FromClause, SqlDialect};
/// use vlorql_core::validate::ValidatedPlan;
/// use std::sync::Arc;
///
/// fn example(compiler: &dyn SqlCompiler, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
///     compiler.compile(plan)
/// }
/// ```
pub trait SqlCompiler: Send + Sync {
    /// Renders the validated plan into a [`CompiledQuery`].
    ///
    /// # Errors
    ///
    /// Returns a [`VlorQLError::Compilation`] when the plan contains
    /// an identifier or function name that cannot be safely emitted
    /// for the target dialect.
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError>;

    /// Returns the SQL dialect emitted by this compiler.
    fn dialect(&self) -> SqlDialect;
}
