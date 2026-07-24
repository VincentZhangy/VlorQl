use super::{CompiledQuery, DialectConfig, QueryBuilder, SqlCompiler};
use crate::errors::VlorQLError;
use crate::schema::SqlDialect;
use crate::validate::ValidatedPlan;

#[derive(Debug, Clone, Copy, Default)]
pub struct MySQLCompiler;

impl SqlCompiler for MySQLCompiler {
    fn compile(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let config = DialectConfig::default_mysql();
        let (sql, parameters) = QueryBuilder::new(plan, &config).build()?;
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
