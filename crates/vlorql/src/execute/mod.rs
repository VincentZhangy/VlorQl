//! Database executors for running compiled SQL queries.
//!
//! This module provides concrete implementations of the
//! [`DatabaseExecutor`] trait for supported database backends.
//!
//! # Feature flags
//!
//! * `executor-postgres` — enables the PostgreSQL executor
//!   ([`PgExecutor`]).

#[cfg(feature = "executor-postgres")]
pub mod pg;
#[cfg(feature = "executor-postgres")]
pub use pg::PgExecutor;
