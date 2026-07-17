//! MySQL compiler implementation.

use super::{CompiledQuery, QueryBuilder, SqlCompiler};
use crate::errors::VlorQLError;
use crate::schema::{IdentifierQuoting, SqlDialect};
use crate::validate::ValidatedPlan;

/// Compiles plans using MySQL backticks and positional `?` placeholders.
#[derive(Debug, Clone, Copy, Default)]
pub struct MySQLCompiler;

impl SqlCompiler for MySQLCompiler {
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let (sql, parameters) =
            QueryBuilder::new(plan, SqlDialect::MySql, IdentifierQuoting::Backtick).build()?;
        Ok(CompiledQuery {
            sql,
            parameters,
            dialect: self.dialect(),
        })
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::MySql
    }
}
