use std::fmt::Write;

use serde_json::{Value, json};

use crate::errors::VlorQLError;
use crate::schema::{
    DataType, FromClause, JoinType, Projection, QueryPlan, SetOperation, SetOperationClause,
    SqlDialect,
};

use super::{QueryBuilder, compilation_error, formatting_error, MYSQL_UNLIMITED_LIMIT};

impl<'a> QueryBuilder<'a> {
    pub(crate) fn build_query(
        &mut self,
        plan: &QueryPlan,
        sql: &mut String,
    ) -> Result<(), VlorQLError> {
        self.build_query_impl(plan, sql, false)
    }

    pub(crate) fn build_query_impl(
        &mut self,
        plan: &QueryPlan,
        sql: &mut String,
        is_set_operand: bool,
    ) -> Result<(), VlorQLError> {
        self.push_alias_scope(plan);
        self.build_with(plan, sql)?;
        self.build_select(plan, sql)?;
        self.build_from(plan, sql)?;
        self.build_where(plan, sql)?;
        self.build_group_by(plan, sql)?;
        self.build_having(plan, sql)?;

        if let Some(set_op) = &plan.set_operation {
            self.render_set_operation(set_op, sql)?;
        }

        if !is_set_operand {
            self.build_order_by(plan, sql)?;
            self.build_limit_offset(plan, sql)?;
        }

        self.alias_stack.pop();
        Ok(())
    }

    fn build_with(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        let Some(ctes) = plan.ctes.as_ref().filter(|ctes| !ctes.is_empty()) else {
            return Ok(());
        };

        if ctes.iter().any(|cte| cte.recursive) {
            sql.push_str("WITH RECURSIVE ");
        } else {
            sql.push_str("WITH ");
        }
        for (index, cte) in ctes.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            let name = self.quote_identifier(&cte.name)?;
            write!(sql, "{name} AS (").map_err(formatting_error)?;
            let saved = self.in_cte;
            self.in_cte = true;
            self.build_query(&cte.query, sql)?;
            self.in_cte = saved;
            sql.push(')');
        }
        sql.push(' ');
        Ok(())
    }

    fn build_select(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        if plan.select.is_empty() {
            return Err(compilation_error(
                "empty_select_list",
                json!({"clause": "select"}),
            ));
        }

        sql.push_str("SELECT ");
        if plan.distinct {
            sql.push_str("DISTINCT ");
            if let Some(on) = &plan.distinct_on {
                if self.dialect != SqlDialect::Postgres {
                    return Err(compilation_error(
                        "unsupported_distinct_on",
                        json!({"dialect": self.config.name, "feature": "DISTINCT ON"}),
                    ));
                }
                sql.push_str("ON (");
                for (i, expr) in on.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    self.render_expression_to(expr, sql)?;
                }
                sql.push_str(") ");
            }
        }
        for (index, projection) in plan.select.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            match projection {
                Projection::Star { table: None } => sql.push('*'),
                Projection::Star { table: Some(table) } => {
                    let max_depth = if self.in_cte { None } else { Some(1) };
                    let resolved = self.resolve_alias(table, max_depth);
                    write!(sql, "{}.*", self.quote_identifier(&resolved)?)
                        .map_err(formatting_error)?;
                }
                Projection::Column {
                    table,
                    column,
                    alias,
                } => {
                    let max_depth = if self.in_cte { None } else { Some(1) };
                    sql.push_str(&self.render_qualified_identifier(
                        table.as_deref(),
                        column,
                        max_depth,
                    )?);
                    if let Some(alias) = alias {
                        write!(sql, " AS {}", self.quote_identifier(alias)?)
                            .map_err(formatting_error)?;
                    }
                }
                Projection::Expr { expression, alias } => {
                    sql.push_str(&self.render_expression(expression)?);
                    if let Some(alias) = alias {
                        write!(sql, " AS {}", self.quote_identifier(alias)?)
                            .map_err(formatting_error)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn build_from(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        sql.push_str(" FROM ");
        sql.push_str(&self.render_from_clause(&plan.from)?);

        if let Some(joins) = &plan.joins {
            for join in joins {
                write!(
                    sql,
                    " {} {}",
                    self.render_join_type(join.join_type)?,
                    self.render_from_clause(&join.right_table)?
                )
                .map_err(formatting_error)?;
                if join.join_type != JoinType::Cross {
                    let condition = self.render_predicate(&join.on)?;
                    write!(sql, " ON {condition}").map_err(formatting_error)?;
                }
            }
        }
        Ok(())
    }

    fn build_where(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        if let Some(predicate) = &plan.r#where {
            let predicate = self.render_predicate(predicate)?;
            write!(sql, " WHERE {predicate}").map_err(formatting_error)?;
        }
        Ok(())
    }

    fn build_group_by(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        let Some(expressions) = plan
            .group_by
            .as_ref()
            .filter(|expressions| !expressions.is_empty())
        else {
            return Ok(());
        };

        let mut rendered = Vec::with_capacity(expressions.len());
        for expression in expressions {
            rendered.push(self.render_expression(expression)?);
        }
        write!(sql, " GROUP BY {}", rendered.join(", ")).map_err(formatting_error)
    }

    fn build_having(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        if let Some(predicate) = &plan.having {
            let predicate = self.render_predicate(predicate)?;
            write!(sql, " HAVING {predicate}").map_err(formatting_error)?;
        }
        Ok(())
    }

    fn build_order_by(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        let Some(terms) = plan.order_by.as_ref().filter(|terms| !terms.is_empty()) else {
            return Ok(());
        };

        let mut rendered = Vec::with_capacity(terms.len());
        for term in terms {
            let expression = self.render_expression(&term.expr)?;
            let direction = if term.descending { "DESC" } else { "ASC" };
            rendered.push(format!("{expression} {direction}"));
        }
        write!(sql, " ORDER BY {}", rendered.join(", ")).map_err(formatting_error)
    }

    fn build_limit_offset(
        &mut self,
        plan: &QueryPlan,
        sql: &mut String,
    ) -> Result<(), VlorQLError> {
        match (self.dialect, plan.limit, plan.offset) {
            (SqlDialect::MySql, Some(limit), Some(offset)) => {
                let offset_ph = self.add_parameter(Value::from(offset), DataType::Int);
                let limit_ph = self.add_parameter(Value::from(limit), DataType::Int);
                write!(sql, " LIMIT {offset_ph}, {limit_ph}").map_err(formatting_error)
            }
            (SqlDialect::MySql, Some(limit), None) => {
                let limit_ph = self.add_parameter(Value::from(limit), DataType::Int);
                write!(sql, " LIMIT {limit_ph}").map_err(formatting_error)
            }
            (SqlDialect::MySql, None, Some(offset)) => {
                let offset_ph = self.add_parameter(Value::from(offset), DataType::Int);
                write!(sql, " LIMIT {offset_ph}, {MYSQL_UNLIMITED_LIMIT}").map_err(formatting_error)
            }
            (SqlDialect::Sqlite, None, Some(offset)) => {
                let offset_ph = self.add_parameter(Value::from(offset), DataType::Int);
                write!(sql, " LIMIT -1 OFFSET {offset_ph}").map_err(formatting_error)
            }
            (_, limit, offset) => {
                if let Some(limit) = limit {
                    let limit_ph = self.add_parameter(Value::from(limit), DataType::Int);
                    write!(sql, " LIMIT {limit_ph}").map_err(formatting_error)?;
                }
                if let Some(offset) = offset {
                    let offset_ph = self.add_parameter(Value::from(offset), DataType::Int);
                    write!(sql, " OFFSET {offset_ph}").map_err(formatting_error)?;
                }
                Ok(())
            }
        }
    }

    pub(crate) fn render_from_clause(&mut self, from: &FromClause) -> Result<String, VlorQLError> {
        match from {
            FromClause::Table { table, alias } => {
                let table = self.quote_identifier(table)?;
                match alias {
                    Some(alias) => Ok(format!("{table} AS {}", self.quote_identifier(alias)?)),
                    None => Ok(table),
                }
            }
            FromClause::Subquery { query, alias } => {
                let mut sql = String::from("(");
                self.build_query(query, &mut sql)?;
                sql.push(')');
                if let Some(alias) = alias {
                    write!(sql, " AS {}", self.quote_identifier(alias)?)
                        .map_err(formatting_error)?;
                }
                Ok(sql)
            }
        }
    }

    pub(crate) fn render_qualified_identifier(
        &self,
        qualifier: Option<&str>,
        identifier: &str,
        max_depth: Option<usize>,
    ) -> Result<String, VlorQLError> {
        let identifier = self.quote_identifier(identifier)?;
        match qualifier {
            Some(qualifier) => {
                let resolved = self.resolve_alias(qualifier, max_depth);
                Ok(format!(
                    "{}.{}",
                    self.quote_identifier(&resolved)?,
                    identifier
                ))
            }
            None => Ok(identifier),
        }
    }

    fn quote_identifier(&self, identifier: &str) -> Result<String, VlorQLError> {
        if identifier.is_empty() {
            return Err(compilation_error(
                "empty_identifier",
                json!({"identifier": identifier}),
            ));
        }

        let style = self.config.identifier_quote.as_str();
        if style == "never" {
            validate_unquoted_identifier(identifier)?;
            Ok(identifier.to_owned())
        } else {
            Ok(self.config.quote_identifier(identifier))
        }
    }

    fn render_set_operation(
        &mut self,
        set_op: &SetOperationClause,
        sql: &mut String,
    ) -> Result<(), VlorQLError> {
        let keyword = match set_op.operation {
            SetOperation::UnionAll => " UNION ALL ",
            SetOperation::Union => " UNION ",
            SetOperation::Intersect => " INTERSECT ",
            SetOperation::Except => " EXCEPT ",
        };
        sql.push_str(keyword);
        self.build_query_impl(&set_op.right, sql, true)
    }

    fn render_join_type(&self, join_type: JoinType) -> Result<&'static str, VlorQLError> {
        let is_mysql =
            self.dialect == SqlDialect::MySql || self.config.name.to_lowercase().contains("mysql");
        match join_type {
            JoinType::Full if is_mysql => Err(compilation_error(
                "unsupported_full_join",
                json!({"dialect": self.config.name, "join_type": "full"}),
            )),
            JoinType::Inner => Ok("INNER JOIN"),
            JoinType::Left => Ok("LEFT JOIN"),
            JoinType::Right => Ok("RIGHT JOIN"),
            JoinType::Full => Ok("FULL JOIN"),
            JoinType::Cross => Ok("CROSS JOIN"),
        }
    }
}

fn validate_unquoted_identifier(identifier: &str) -> Result<(), VlorQLError> {
    let mut characters = identifier.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    let valid_rest =
        characters.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if !valid_start || !valid_rest {
        return Err(compilation_error(
            "invalid_unquoted_identifier",
            json!({"identifier": identifier}),
        ));
    }
    if is_reserved_keyword(identifier) {
        return Err(compilation_error(
            "reserved_keyword_unquoted",
            json!({"identifier": identifier}),
        ));
    }
    Ok(())
}

/// Standard SQL reserved keywords.  Sorted alphabetically for binary search.
static RESERVED_KEYWORDS: &[&str] = &[
    "ALL", "AND", "AS", "BETWEEN", "BY", "CASE", "CROSS", "DELETE", "DESC", "DISTINCT", "DROP",
    "ELSE", "END", "ESCAPE", "EXCEPT", "EXISTS", "FALSE", "FROM", "FULL", "GROUP", "HAVING", "IN",
    "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "LEFT", "LIKE", "LIMIT", "NOT",
    "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "RIGHT", "SELECT", "SET", "TABLE", "THEN",
    "TRUE", "UNION", "UNIQUE", "UPDATE", "VALUES", "WHEN", "WHERE", "WITH",
];

/// Returns `true` when `ident` is a SQL reserved keyword (case-insensitive).
fn is_reserved_keyword(ident: &str) -> bool {
    RESERVED_KEYWORDS
        .binary_search(&ident.to_uppercase().as_str())
        .is_ok()
}
