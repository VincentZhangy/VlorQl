//! Generic sqlx-based database executor.
//!
//! Provides a generic [`SqlxExecutor<P>`] that works with any sqlx pool
//! type (MySQL, SQLite, …) via the [`SqlxPool`] trait.

use async_trait::async_trait;
use sqlx::{Column, Database, Row};
use vlorql_core::compile::CompiledQuery;
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::{DatabaseExecutor, QueryResult};

/// Trait abstracting over sqlx pool types so that [`SqlxExecutor`] can
/// be generic.
#[async_trait]
pub trait SqlxPool: Sized + Send + Sync {
    type DB: Database;
    type Options: Default + Send;
    type DbRow: Row;

    async fn connect(url: &str, opts: Self::Options) -> Result<Self, sqlx::Error>;

    async fn fetch_all(pool: &Self, sql: &str) -> Result<Vec<Self::DbRow>, sqlx::Error>;

    /// Convert a single cell from a database row into a JSON value.
    fn row_to_values(row: &Self::DbRow, i: usize) -> serde_json::Value;
}

#[async_trait]
impl SqlxPool for sqlx::MySqlPool {
    type DB = sqlx::MySql;
    type Options = sqlx::mysql::MySqlPoolOptions;
    type DbRow = sqlx::mysql::MySqlRow;

    async fn connect(url: &str, opts: Self::Options) -> Result<Self, sqlx::Error> {
        opts.max_connections(5).connect(url).await
    }

    async fn fetch_all(pool: &Self, sql: &str) -> Result<Vec<Self::DbRow>, sqlx::Error> {
        sqlx::query(sql).fetch_all(pool).await
    }

    fn row_to_values(row: &Self::DbRow, i: usize) -> serde_json::Value {
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
}

#[async_trait]
impl SqlxPool for sqlx::SqlitePool {
    type DB = sqlx::Sqlite;
    type Options = sqlx::sqlite::SqlitePoolOptions;
    type DbRow = sqlx::sqlite::SqliteRow;

    async fn connect(url: &str, opts: Self::Options) -> Result<Self, sqlx::Error> {
        opts.max_connections(5).connect(url).await
    }

    async fn fetch_all(pool: &Self, sql: &str) -> Result<Vec<Self::DbRow>, sqlx::Error> {
        sqlx::query(sql).fetch_all(pool).await
    }

    fn row_to_values(row: &Self::DbRow, i: usize) -> serde_json::Value {
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
}

/// Generic sqlx-based executor parameterized by a pool type.
pub struct SqlxExecutor<P: SqlxPool> {
    pool: P,
}

impl<P: SqlxPool> SqlxExecutor<P> {
    /// Creates a new executor by connecting to the given database URL.
    pub async fn new(database_url: &str) -> Result<Self, VlorQLError> {
        let pool = P::connect(database_url, P::Options::default())
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("failed to connect: {e}"),
                    },
                    serde_json::json!({}),
                )
            })?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl<P: SqlxPool + 'static> DatabaseExecutor for SqlxExecutor<P> {
    async fn execute(&self, query: &CompiledQuery) -> Result<QueryResult, VlorQLError> {
        let rows = P::fetch_all(&self.pool, &query.sql)
            .await
            .map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: "database".into(),
                        reason: format!("query failed: {e}"),
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
                    .map(|(i, _)| P::row_to_values(row, i))
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
