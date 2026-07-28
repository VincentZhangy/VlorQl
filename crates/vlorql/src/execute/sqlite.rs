//! SQLite executor (type alias for the generic sqlx executor).

/// Executes compiled SQL queries against a SQLite database.
///
/// # Example
///
/// ```ignore
/// use vlorql::execute::SqliteExecutor;
/// use vlorql_core::execute::DatabaseExecutor;
///
/// let executor = SqliteExecutor::new("sqlite:///path/to/db.db")
///     .await
///     .expect("connect");
/// ```
pub type SqliteExecutor = crate::execute::sqlx_executor::SqlxExecutor<sqlx::SqlitePool>;
