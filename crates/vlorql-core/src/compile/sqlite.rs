//! SQLite compiler implementation.

use super::{CompiledQuery, QueryBuilder, SqlCompiler};
use crate::errors::VlorQLError;
use crate::schema::{IdentifierQuoting, SqlDialect};
use crate::validate::ValidatedPlan;

/// Compiles plans using SQLite quoting and positional `?` placeholders.
#[derive(Debug, Clone, Copy, Default)]
pub struct SQLiteCompiler;

impl SqlCompiler for SQLiteCompiler {
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let (sql, parameters) =
            QueryBuilder::new(plan, SqlDialect::Sqlite, IdentifierQuoting::DoubleQuote).build()?;
        Ok(CompiledQuery {
            sql,
            parameters,
            dialect: self.dialect(),
        })
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }
}
