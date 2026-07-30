//! Dialect-aware parameterized SQL construction.

use super::dialect_config::{DialectConfig, dialect_from_name};
use super::types::Parameter;
use crate::errors::{CompilationErrorKind, VlorQLError};
use crate::schema::{DataType, Expression, FromClause, Predicate, QueryPlan, SqlDialect};
use crate::validate::ValidatedPlan;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;

pub(crate) mod clause;
pub(crate) mod expr;

/// MySQL's maximum unsigned bigint value, used as a sentinel to represent
/// "no limit" when only OFFSET is specified (LIMIT offset, <unlimited>).
const MYSQL_UNLIMITED_LIMIT: u64 = 18_446_744_073_709_551_615;

/// Builds SQL while preserving the exact textual order of bind parameters.
pub struct QueryBuilder<'a> {
    plan: &'a ValidatedPlan,
    config: &'a DialectConfig,
    dialect: SqlDialect,
    parameters: Vec<Parameter>,
    alias_stack: Vec<HashMap<String, String>>,
    param_cache: HashMap<(String, DataType), usize>,
    in_cte: bool,
}

impl<'a> QueryBuilder<'a> {
    /// Creates a builder for one validated plan and dialect config.
    ///
    /// # Errors
    ///
    /// Returns `VlorQLError` when `config.name` does not match any known
    /// SQL dialect.
    pub fn new(plan: &'a ValidatedPlan, config: &'a DialectConfig) -> Result<Self, VlorQLError> {
        let dialect = dialect_from_name(&config.name).ok_or_else(|| {
            VlorQLError::compilation(
                CompilationErrorKind::UnsupportedDialectFeature {
                    feature: format!("unknown dialect '{}'", config.name),
                },
                json!({"name": &config.name}),
            )
        })?;
        let mut builder = Self {
            plan,
            config,
            dialect,
            parameters: Vec::new(),
            alias_stack: Vec::new(),
            param_cache: HashMap::new(),
            in_cte: false,
        };
        builder.push_alias_scope(plan.as_plan());
        Ok(builder)
    }

    fn push_alias_scope(&mut self, plan: &QueryPlan) {
        let mut map = HashMap::new();
        Self::collect_aliases(&plan.from, &mut map);
        if let Some(joins) = &plan.joins {
            for join in joins {
                Self::collect_aliases(&join.right_table, &mut map);
            }
        }
        self.alias_stack.push(map);
    }

    fn collect_aliases(from: &FromClause, map: &mut HashMap<String, String>) {
        match from {
            FromClause::Table { table, alias } => {
                let effective = alias.clone().unwrap_or_else(|| table.clone());
                map.entry(table.clone())
                    .or_insert_with(|| effective.clone());
                if let Some(alias) = alias {
                    map.insert(alias.clone(), effective);
                }
            }
            FromClause::Subquery { alias, .. } => {
                if let Some(alias) = alias {
                    map.insert(alias.clone(), alias.clone());
                }
            }
        }
    }

    fn resolve_alias<'b>(&self, qualifier: &'b str, max_depth: Option<usize>) -> Cow<'b, str> {
        self.alias_stack
            .iter()
            .rev()
            .enumerate()
            .take_while(|(i, _)| max_depth.is_none_or(|d| *i < d))
            .find_map(|(_, map)| map.get(qualifier))
            .map(|s| Cow::Owned(s.clone()))
            .unwrap_or(Cow::Borrowed(qualifier))
    }

    /// Builds a SQL string and returns its parameters in placeholder order.
    pub fn build(mut self) -> Result<(String, Vec<Parameter>), VlorQLError> {
        if dialect_from_name(&self.config.name).is_none() {
            return Err(compilation_error(
                "unknown_dialect",
                json!({"dialect": self.config.name, "accepted": ["postgres", "sqlite", "mysql"]}),
            ));
        }
        tracing::event!(tracing::Level::DEBUG, "Building SQL from QueryPlan");
        let plan = self.plan.as_plan();
        let mut sql = String::new();
        self.build_query(plan, &mut sql)?;
        Ok((sql, self.parameters))
    }

    /// Renders one expression and appends any literal parameters to this builder.
    pub fn render_expression(&mut self, expression: &Expression) -> Result<String, VlorQLError> {
        let mut buf = String::new();
        self.render_expression_to(expression, &mut buf)?;
        Ok(buf)
    }

    /// Renders one predicate and appends literal values as bind parameters.
    pub fn render_predicate(&mut self, predicate: &Predicate) -> Result<String, VlorQLError> {
        let mut buf = String::new();
        self.render_predicate_to(predicate, &mut buf)?;
        Ok(buf)
    }

    /// Adds a parameter and returns the placeholder for the selected dialect.
    pub fn add_parameter(&mut self, value: Value, data_type: DataType) -> String {
        let val_str = serde_json::to_string(&value).unwrap_or_default();
        let key = (val_str, data_type);

        if let Some(&idx) = self.param_cache.get(&key) {
            if self.config.placeholder.contains("{index}") {
                return self.config.placeholder_str(idx + 1);
            }
            return self.config.placeholder_str(0);
        }

        let idx = self.parameters.len();
        self.parameters.push(Parameter { value, data_type });
        self.param_cache.insert(key, idx);
        self.config.placeholder_str(idx + 1)
    }

    /// Returns the dialect selected for this builder.
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }
}

fn compilation_error(feature: impl Into<String>, details: Value) -> VlorQLError {
    VlorQLError::compilation(
        CompilationErrorKind::UnsupportedDialectFeature {
            feature: feature.into(),
        },
        details,
    )
}

fn formatting_error(_error: std::fmt::Error) -> VlorQLError {
    compilation_error("sql_formatting", json!({"reason": "formatting_failed"}))
}
