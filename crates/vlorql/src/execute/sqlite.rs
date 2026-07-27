//! SQLite executor implementation.
//!
//! Wraps a [`sqlx::SqlitePool`] and implements the [`DatabaseExecutor`]
//! trait so that compiled SQL queries can be executed against a SQLite
//! database.

use async_trait::async_trait;
use sqlx::Column;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use vlorql_core::compile::CompiledQuery;
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::{DatabaseExecutor, QueryResult};

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
pub struct SqliteExecutor {
    pool: SqlitePool,
}

impl SqliteExecutor {
    /// Creates a new SQLite executor by connecting to the given
    /// database URL.
    pub async fn new(database_url: &str) -> Result<Self, VlorQLError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("failed to connect to SQLite: {e}"),
                    },
                    serde_json::json!({}),
                )
            })?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl DatabaseExecutor for SqliteExecutor {
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
        let rows = sqlx::query(&query.sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("SQLite query failed: {e}"),
                    },
                    serde_json::json!({}),
                )
            })?;

        let columns: Vec<String> = if rows.is_empty() {
            Vec::new()
        } else {
            rows[0]
                .columns()
                .iter()
                .map(|c| c.name().to_owned())
                .collect()
        };

        let rows_affected = rows.len() as u64;

        let values: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, _)| row_to_values(row, i))
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: values,
            rows_affected,
        })
    }
}

/// Converts a SQLite row cell at the given index into a
/// [`serde_json::Value`].
///
/// Supports the following native types: `i32`, `i64`, `f64`,
/// `String`, `bool`. All other types fall back to `Null`.
fn row_to_values(row: &sqlx::sqlite::SqliteRow, i: usize) -> serde_json::Value {
    if let Ok(v) = row.try_get::<i32, _>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<i64, _>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<f64, _>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<String, _>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<bool, _>(i) {
        return serde_json::json!(v);
    }
    serde_json::Value::Null
}
