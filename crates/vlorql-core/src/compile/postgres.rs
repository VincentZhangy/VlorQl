//! PostgreSQL compiler implementation.

use super::{CompiledQuery, QueryBuilder, SqlCompiler};
use crate::errors::VlorQLError;
use crate::schema::{IdentifierQuoting, SqlDialect};
use crate::validate::ValidatedPlan;

/// Compiles plans using PostgreSQL quoting and numbered placeholders.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresCompiler;

impl SqlCompiler for PostgresCompiler {
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let (sql, parameters) =
            QueryBuilder::new(plan, SqlDialect::Postgres, IdentifierQuoting::DoubleQuote)
                .build()?;
        Ok(CompiledQuery {
            sql,
            parameters,
            dialect: self.dialect(),
        })
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}
