//! Dialect-aware parameterized SQL construction.

/// MySQL's maximum unsigned bigint value, used as a sentinel to represent
/// "no limit" when only OFFSET is specified (LIMIT offset, <unlimited>).
const MYSQL_UNLIMITED_LIMIT: u64 = 18446744073709551615;

use super::types::Parameter;
use crate::errors::{CompilationErrorKind, VlorQLError};
use crate::schema::{
    BinaryOperator, ComparisonOperator, DataType, Expression, FromClause, IdentifierQuoting,
    InTarget, JoinType, Predicate, Projection, QueryPlan, SqlDialect,
};
use crate::validate::ValidatedPlan;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fmt::Write;

/// Builds SQL while preserving the exact textual order of bind parameters.
pub struct QueryBuilder<'a> {
    plan: &'a ValidatedPlan,
    dialect: SqlDialect,
    parameters: Vec<Parameter>,
    quote_style: IdentifierQuoting,
}

impl<'a> QueryBuilder<'a> {
    /// Creates a builder for one validated plan and output dialect.
    pub fn new(
        plan: &'a ValidatedPlan,
        dialect: SqlDialect,
        quote_style: IdentifierQuoting,
    ) -> Self {
        Self {
            plan,
            dialect,
            parameters: Vec::new(),
            quote_style,
        }
    }

    /// Builds a SQL string and returns its parameters in placeholder order.
    pub fn build(mut self) -> Result<(String, Vec<Parameter>), VlorQLError> {
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

    /// Renders one expression into `buf`, avoiding intermediate allocations.
    pub fn render_expression_to(
        &mut self,
        expression: &Expression,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match expression {
            Expression::Literal { value, data_type } => {
                buf.push_str(&self.add_parameter(value.clone(), *data_type));
                Ok(())
            }
            Expression::ColumnRef { table, column } => {
                buf.push_str(&self.render_qualified_identifier(table.as_deref(), column)?);
                Ok(())
            }
            Expression::FunctionCall {
                name,
                args,
                distinct,
            } => {
                let function = self.render_function_name(name)?;
                buf.push_str(&function);
                buf.push('(');
                if *distinct {
                    buf.push_str("DISTINCT ");
                }
                for (i, argument) in args.iter().enumerate() {
                    if i > 0 {
                        buf.push_str(", ");
                    }
                    self.render_expression_to(argument, buf)?;
                }
                buf.push(')');
                Ok(())
            }
            Expression::BinaryOp { left, op, right } => {
                buf.push('(');
                self.render_expression_to(left, buf)?;
                write!(buf, " {} ", self.render_binary_operator(*op)).map_err(formatting_error)?;
                self.render_expression_to(right, buf)?;
                buf.push(')');
                Ok(())
            }
            Expression::Star => {
                buf.push('*');
                Ok(())
            }
            Expression::SubQuery { query } => {
                buf.push('(');
                self.build_query(query, buf)?;
                buf.push(')');
                Ok(())
            }
        }
    }

    /// Renders one predicate and appends literal values as bind parameters.
    pub fn render_predicate(&mut self, predicate: &Predicate) -> Result<String, VlorQLError> {
        let mut buf = String::new();
        self.render_predicate_to(predicate, &mut buf)?;
        Ok(buf)
    }

    /// Renders one predicate into `buf`, avoiding intermediate allocations.
    pub fn render_predicate_to(
        &mut self,
        predicate: &Predicate,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match predicate {
            Predicate::Comparison { left, op, right } => {
                self.render_expression_to(left, buf)?;
                write!(buf, " {} ", self.render_comparison_operator(*op)?)
                    .map_err(formatting_error)?;
                self.render_expression_to(right, buf)
            }
            Predicate::And { left, right } => {
                buf.push('(');
                self.render_predicate_to(left, buf)?;
                buf.push_str(") AND (");
                self.render_predicate_to(right, buf)?;
                buf.push(')');
                Ok(())
            }
            Predicate::Or { left, right } => {
                buf.push('(');
                self.render_predicate_to(left, buf)?;
                buf.push_str(") OR (");
                self.render_predicate_to(right, buf)?;
                buf.push(')');
                Ok(())
            }
            Predicate::Not { child } => {
                buf.push_str("NOT (");
                self.render_predicate_to(child, buf)?;
                buf.push(')');
                Ok(())
            }
            Predicate::Between { expr, low, high } => {
                self.render_expression_to(expr, buf)?;
                buf.push_str(" BETWEEN ");
                self.render_expression_to(low, buf)?;
                buf.push_str(" AND ");
                self.render_expression_to(high, buf)
            }
            Predicate::In { expr, target } => {
                self.render_expression_to(expr, buf)?;
                match target {
                    InTarget::Values(values) => {
                        if values.is_empty() {
                            return Err(compilation_error(
                                "empty_in_list",
                                json!({"predicate": "in"}),
                            ));
                        }
                        buf.push_str(" IN (");
                        for (i, value) in values.iter().enumerate() {
                            if i > 0 {
                                buf.push_str(", ");
                            }
                            self.render_expression_to(value, buf)?;
                        }
                        buf.push(')');
                        Ok(())
                    }
                    InTarget::SubQuery(query) => {
                        buf.push_str(" IN (");
                        self.build_query(query, buf)?;
                        buf.push(')');
                        Ok(())
                    }
                }
            }
            Predicate::Exists { query } => {
                buf.push_str("EXISTS (");
                self.build_query(query, buf)?;
                buf.push(')');
                Ok(())
            }
            Predicate::Like { expr, pattern } => {
                self.render_expression_to(expr, buf)?;
                let placeholder =
                    self.add_parameter(Value::String(pattern.clone()), DataType::String);
                write!(buf, " LIKE {placeholder}").map_err(formatting_error)
            }
            Predicate::IsNull { expr } => {
                self.render_expression_to(expr, buf)?;
                buf.push_str(" IS NULL");
                Ok(())
            }
        }
    }

    /// Adds a parameter and returns the placeholder for the selected dialect.
    pub fn add_parameter(&mut self, value: Value, data_type: DataType) -> String {
        self.parameters.push(Parameter { value, data_type });
        match self.dialect {
            SqlDialect::Postgres => format!("${}", self.parameters.len()),
            SqlDialect::Sqlite | SqlDialect::MySql => "?".to_owned(),
        }
    }

    /// Returns the dialect selected for this builder.
    pub fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    fn build_query(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        self.build_with(plan, sql)?;
        self.build_select(plan, sql)?;
        self.build_from(plan, sql)?;
        self.build_where(plan, sql)?;
        self.build_group_by(plan, sql)?;
        self.build_having(plan, sql)?;
        self.build_order_by(plan, sql)?;
        self.build_limit_offset(plan, sql)?;
        Ok(())
    }

    fn build_with(&mut self, plan: &QueryPlan, sql: &mut String) -> Result<(), VlorQLError> {
        let Some(ctes) = plan.ctes.as_ref().filter(|ctes| !ctes.is_empty()) else {
            return Ok(());
        };

        sql.push_str("WITH ");
        for (index, cte) in ctes.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            let name = self.quote_identifier(&cte.name)?;
            write!(sql, "{name} AS (").map_err(formatting_error)?;
            self.build_query(&cte.query, sql)?;
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
        for (index, projection) in plan.select.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            match projection {
                Projection::Star { table: None } => sql.push('*'),
                Projection::Star { table: Some(table) } => {
                    write!(sql, "{}.*", self.quote_identifier(table)?).map_err(formatting_error)?;
                }
                Projection::Column {
                    table,
                    column,
                    alias,
                } => {
                    sql.push_str(&self.render_qualified_identifier(table.as_deref(), column)?);
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

    fn render_from_clause(&self, from: &FromClause) -> Result<String, VlorQLError> {
        let table = self.quote_identifier(&from.table)?;
        match &from.alias {
            Some(alias) => Ok(format!("{table} AS {}", self.quote_identifier(alias)?)),
            None => Ok(table),
        }
    }

    fn render_qualified_identifier(
        &self,
        qualifier: Option<&str>,
        identifier: &str,
    ) -> Result<String, VlorQLError> {
        let identifier = self.quote_identifier(identifier)?;
        match qualifier {
            Some(qualifier) => Ok(format!(
                "{}.{}",
                self.quote_identifier(qualifier)?,
                identifier
            )),
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

        match self.effective_quote_style() {
            IdentifierQuoting::Never => {
                validate_unquoted_identifier(identifier)?;
                Ok(identifier.to_owned())
            }
            IdentifierQuoting::DoubleQuote => {
                Ok(format!("\"{}\"", identifier.replace('"', "\"\"")))
            }
            IdentifierQuoting::Backtick => Ok(format!("`{}`", identifier.replace('`', "``"))),
            IdentifierQuoting::Always => Err(compilation_error(
                "unresolved_quote_style",
                json!({"identifier": identifier}),
            )),
        }
    }

    fn effective_quote_style(&self) -> IdentifierQuoting {
        match self.quote_style {
            IdentifierQuoting::Always => match self.dialect {
                SqlDialect::MySql => IdentifierQuoting::Backtick,
                SqlDialect::Postgres | SqlDialect::Sqlite => IdentifierQuoting::DoubleQuote,
            },
            quote_style => quote_style,
        }
    }

    fn render_function_name<'b>(&self, function: &'b str) -> Result<Cow<'b, str>, VlorQLError> {
        if function.is_empty() {
            return Err(compilation_error(
                "empty_function_name",
                json!({"function": function}),
            ));
        }
        for segment in function.split('.') {
            validate_unquoted_identifier(segment)?;
        }
        Ok(Cow::Borrowed(function))
    }

    fn render_binary_operator(&self, operator: BinaryOperator) -> &'static str {
        match operator {
            BinaryOperator::Add => "+",
            BinaryOperator::Sub => "-",
            BinaryOperator::Mul => "*",
            BinaryOperator::Div => "/",
            BinaryOperator::Mod => "%",
            BinaryOperator::And => "AND",
            BinaryOperator::Or => "OR",
            BinaryOperator::Eq => "=",
            BinaryOperator::Neq => "<>",
            BinaryOperator::Gt => ">",
            BinaryOperator::Lt => "<",
            BinaryOperator::Gte => ">=",
            BinaryOperator::Lte => "<=",
            BinaryOperator::Like => "LIKE",
            BinaryOperator::ILike if self.dialect == SqlDialect::Postgres => "ILIKE",
            BinaryOperator::ILike => "LIKE",
        }
    }

    fn render_comparison_operator(
        &self,
        operator: ComparisonOperator,
    ) -> Result<&'static str, VlorQLError> {
        match operator {
            ComparisonOperator::Eq => Ok("="),
            ComparisonOperator::Neq => Ok("<>"),
            ComparisonOperator::Gt => Ok(">"),
            ComparisonOperator::Lt => Ok("<"),
            ComparisonOperator::Gte => Ok(">="),
            ComparisonOperator::Lte => Ok("<="),
            ComparisonOperator::Like => Ok("LIKE"),
            ComparisonOperator::ILike if self.dialect == SqlDialect::Postgres => Ok("ILIKE"),
            ComparisonOperator::ILike => Ok("LIKE"),
            ComparisonOperator::In => Err(compilation_error(
                "comparison_in_requires_in_predicate",
                json!({"operator": operator}),
            )),
            ComparisonOperator::Between => Err(compilation_error(
                "comparison_between_requires_between_predicate",
                json!({"operator": operator}),
            )),
        }
    }

    fn render_join_type(&self, join_type: JoinType) -> Result<&'static str, VlorQLError> {
        match join_type {
            JoinType::Full if self.dialect == SqlDialect::MySql => Err(compilation_error(
                "unsupported_full_join",
                json!({"dialect": "mysql", "join_type": "full"}),
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
    "ALL",
    "AND",
    "AS",
    "BETWEEN",
    "BY",
    "CASE",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

/// Returns `true` when `ident` is a SQL reserved keyword (case-insensitive).
fn is_reserved_keyword(ident: &str) -> bool {
    RESERVED_KEYWORDS
        .binary_search(&ident.to_uppercase().as_str())
        .is_ok()
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
