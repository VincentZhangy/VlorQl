//! Compiler factory for supported SQL dialects.

use super::{MySQLCompiler, PostgresCompiler, SQLiteCompiler, SqlCompiler};
use crate::schema::SqlDialect;

/// Stateless factory for dialect-specific compilers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompilerRegistry;

impl CompilerRegistry {
    /// Creates a compiler for a supported dialect.
    pub fn get(dialect: SqlDialect) -> Box<dyn SqlCompiler> {
        get_compiler(dialect)
    }
}

/// Creates a compiler for a supported SQL dialect.
pub fn get_compiler(dialect: SqlDialect) -> Box<dyn SqlCompiler> {
    match dialect {
        SqlDialect::Postgres => Box::new(PostgresCompiler),
        SqlDialect::Sqlite => Box::new(SQLiteCompiler),
        SqlDialect::MySql => Box::new(MySQLCompiler),
    }
}
