//! Query execution trait and result types.
//!
//! This module defines [`DatabaseExecutor`], the core abstraction for
//! executing compiled SQL queries against a database backend, and
//! [`QueryResult`], the structured result returned after execution.

use crate::compile::CompiledQuery;
use crate::errors::VlorQLError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The result of executing a compiled SQL query.
///
/// Contains the column names, the returned rows, and the count of
/// rows affected (useful for DML statements such as `INSERT`,
/// `UPDATE`, or `DELETE`).
///
/// # Examples
///
/// ```
/// use vlorql_core::execute::QueryResult;
///
/// let result = QueryResult {
///     columns: vec!["id".to_string(), "name".to_string()],
///     rows: vec![
///         vec![serde_json::json!(1), serde_json::json!("Alice")],
///         vec![serde_json::json!(2), serde_json::json!("Bob")],
///     ],
///     rows_affected: 2,
/// };
/// assert_eq!(result.columns.len(), 2);
/// assert_eq!(result.rows.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// The column names in the order they appear in the result set.
    pub columns: Vec<String>,

    /// The data rows, each element positionally matching `columns`.
    pub rows: Vec<Vec<serde_json::Value>>,

    /// The number of rows affected by the query.
    ///
    /// For `SELECT` queries this is typically the row count of the
    /// result set; for DML statements it reflects the number of
    /// modified rows.
    pub rows_affected: u64,
}

/// Trait for executing compiled SQL queries against a database.
///
/// Implementors are responsible for connecting to a database backend,
/// sending the compiled SQL with its bound parameters, and returning
/// the result as a [`QueryResult`].
///
/// # Example (mock executor)
///
/// ```
/// use vlorql_core::execute::{DatabaseExecutor, QueryResult};
/// use vlorql_core::compile::CompiledQuery;
/// use vlorql_core::errors::VlorQLError;
/// use vlorql_core::schema::SqlDialect;
///
/// struct MockExecutor;
///
/// #[async_trait::async_trait]
/// impl DatabaseExecutor for MockExecutor {
///     async fn execute(&self, _query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
///         Ok(QueryResult {
///             columns: vec!["id".to_string(), "name".to_string()],
///             rows: vec![
///                 vec![serde_json::json!(1), serde_json::json!("Alice")],
///             ],
///             rows_affected: 1,
///         })
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() {
/// let executor = MockExecutor;
/// let query = CompiledQuery {
///     sql: "SELECT id, name FROM users".to_owned(),
///     parameters: vec![],
///     dialect: SqlDialect::Postgres,
/// };
/// let result = executor.execute(&query).await.unwrap();
/// assert_eq!(result.columns, vec!["id", "name"]);
/// assert_eq!(result.rows[0][0], serde_json::json!(1));
/// # }
/// ```
#[async_trait]
pub trait DatabaseExecutor: Send + Sync {
    /// Execute a compiled query and return the result.
    ///
    /// # Arguments
    ///
    /// * `query` - The [`CompiledQuery`] containing SQL, bind
    ///   parameters, and dialect information.
    ///
    /// # Returns
    ///
    /// * `Ok(QueryResult)` on success.
    /// * `Err(VlorQLError)` if execution fails.
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError>;
}
