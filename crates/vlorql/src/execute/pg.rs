//! PostgreSQL executor implementation.
//!
//! Wraps a [`tokio_postgres::Client`] and implements the
//! [`DatabaseExecutor`] trait so that compiled SQL queries can be
//! executed against a PostgreSQL database.

use async_trait::async_trait;
use tokio_postgres::{Client, Row, types::ToSql};
use vlorql_core::compile::{CompiledQuery, Parameter};
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::{DatabaseExecutor, QueryResult};

/// Executes compiled SQL queries against a PostgreSQL database.
///
/// # Example
///
/// ```ignore
/// use vlorql::execute::PgExecutor;
/// use vlorql_core::execute::DatabaseExecutor;
///
/// let (client, connection) = tokio_postgres::connect(
///     "host=localhost user=postgres", tokio_postgres::NoTls,
/// )
/// .await
/// .expect("connect");
/// tokio::spawn(async move { connection.await.expect("connection") });
///
/// let executor = PgExecutor::new(client);
/// ```
pub struct PgExecutor {
    client: Client,
}

impl PgExecutor {
    /// Creates a new PostgreSQL executor wrapping an existing
    /// [`tokio_postgres::Client`].
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

/// Owned parameter value that can be borrowed as `&dyn ToSql + Sync`.
enum OwnedParam {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    /// SQL NULL, storing `None::<i32>` so we can borrow it.
    Null(Option<i32>),
}

impl OwnedParam {
    /// Borrow this value as a `&dyn ToSql + Sync` reference.
    fn as_dyn_ref(&self) -> &(dyn ToSql + Sync) {
        match self {
            OwnedParam::Int(v) => v,
            OwnedParam::Float(v) => v,
            OwnedParam::Text(v) => v,
            OwnedParam::Bool(v) => v,
            OwnedParam::Null(v) => v as &(dyn ToSql + Sync),
        }
    }
}

/// Converts a [`Parameter`] into an [`OwnedParam`].
fn param_to_owned(p: &Parameter) -> OwnedParam {
    match &p.value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() {
                    OwnedParam::Int(n.as_i64().unwrap_or(0))
                } else {
                    OwnedParam::Float(f)
                }
            } else {
                OwnedParam::Int(n.as_i64().unwrap_or(0))
            }
        }
        serde_json::Value::String(s) => OwnedParam::Text(s.clone()),
        serde_json::Value::Bool(b) => OwnedParam::Bool(*b),
        serde_json::Value::Null => OwnedParam::Null(None),
        // Arrays and objects are serialised as JSON strings.
        other => OwnedParam::Text(other.to_string()),
    }
}

#[async_trait]
impl DatabaseExecutor for PgExecutor {
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
        let statement = self.client.prepare(&query.sql).await.map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: "database".into(),
                    reason: format!("failed to prepare SQL: {e}"),
                },
                serde_json::json!({}),
            )
        })?;

        // Convert parameters to owned values and collect references.
        let param_values: Vec<OwnedParam> = query.parameters.iter().map(param_to_owned).collect();
        let params: Vec<&(dyn ToSql + Sync)> =
            param_values.iter().map(|p| p.as_dyn_ref()).collect();

        let rows = self.client.query(&statement, &params).await.map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: "database".into(),
                    reason: format!("query execution failed: {e}"),
                },
                serde_json::json!({}),
            )
        })?;

        let columns: Vec<String> = statement
            .columns()
            .iter()
            .map(|col| col.name().to_owned())
            .collect();

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

/// Converts a PostgreSQL row cell at the given index into a
/// [`serde_json::Value`].
///
/// Supports the following native types:
/// * `i32`, `i64`
/// * `f64`
/// * `String` / `&str`
/// * `bool`
///
/// All other types fall back to a debug-representation string.
fn row_to_values(row: &Row, i: usize) -> serde_json::Value {
    if let Ok(v) = row.try_get::<_, i32>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<_, i64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<_, f64>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<_, String>(i) {
        return serde_json::json!(v);
    }
    if let Ok(v) = row.try_get::<_, bool>(i) {
        return serde_json::json!(v);
    }
    // Fallback: try to get as a string representation.
    if let Ok(v) = row.try_get::<_, String>(i) {
        serde_json::json!(v)
    } else {
        serde_json::Value::Null
    }
}
