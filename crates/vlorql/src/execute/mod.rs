//! Database executors for running compiled SQL queries.
//!
//! This module provides concrete implementations of the
//! [`DatabaseExecutor`] trait for supported database backends.
//!
//! # Feature flags
//!
//! * `executor-postgres` — enables the PostgreSQL executor
//!   ([`PgExecutor`]).
//! * `executor-mysql` — enables the MySQL executor
//!   ([`MysqlExecutor`]).
//! * `executor-sqlite` — enables the SQLite executor
//!   ([`SqliteExecutor`]).

#[cfg(feature = "executor-postgres")]
pub mod pg;
#[cfg(feature = "executor-postgres")]
pub use pg::PgExecutor;

#[cfg(feature = "executor-mysql")]
pub mod mysql;
#[cfg(feature = "executor-mysql")]
pub use mysql::MysqlExecutor;

#[cfg(feature = "executor-sqlite")]
pub mod sqlite;
#[cfg(feature = "executor-sqlite")]
pub use sqlite::SqliteExecutor;
