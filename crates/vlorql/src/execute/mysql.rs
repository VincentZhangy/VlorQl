//! MySQL executor (type alias for the generic sqlx executor).

/// Executes compiled SQL queries against a MySQL database.
///
/// # Example
///
/// ```ignore
/// use vlorql::execute::MysqlExecutor;
/// use vlorql_core::execute::DatabaseExecutor;
///
/// let executor = MysqlExecutor::new("mysql://user:pass@localhost/db")
///     .await
///     .expect("connect");
/// ```
pub type MysqlExecutor = crate::execute::sqlx_executor::SqlxExecutor<sqlx::MySqlPool>;
