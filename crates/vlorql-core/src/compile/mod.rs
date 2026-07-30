//! Safe, parameterized SQL compilation for supported dialects.

#![allow(missing_docs)]

pub mod builder;
pub mod dialect_config;
pub mod mysql;
pub mod postgres;
pub mod registry;
pub mod rewrite;
pub mod sqlite;
pub mod types;

pub use builder::QueryBuilder;
pub use dialect_config::DialectConfig;
pub use mysql::MySQLCompiler;
pub use postgres::PostgresCompiler;
pub use registry::{CompilerRegistry, DialectRegistry, get_compiler};
pub use rewrite::{RewriteEngine, RewriteRule};
pub use sqlite::SQLiteCompiler;
pub use types::{CompiledQuery, Parameter, SqlCompiler};

#[cfg(test)]
mod tests;
