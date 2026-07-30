use std::borrow::Cow;
use std::fmt::Write;

use serde_json::{Value, json};

use crate::errors::VlorQLError;
use crate::schema::{
    BinaryOperator, ComparisonOperator, DataType, Expression, InTarget, Predicate, SqlDialect,
    WindowFrame, WindowFrameBound, WindowFrameKind, WindowSpec,
};

use super::{QueryBuilder, compilation_error, formatting_error};

impl<'a> QueryBuilder<'a> {
    pub(crate) fn render_expression_to(
        &mut self,
        expression: &Expression,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match expression {
            Expression::Literal { value, data_type } => {
                let placeholder = self.add_parameter(value.clone(), *data_type);
                if self.in_cte {
                    self.render_cte_cast(&placeholder, *data_type, buf)?;
                } else {
                    buf.push_str(&placeholder);
                }
                Ok(())
            }
            Expression::ColumnRef { table, column } => {
                let max_depth = if self.in_cte { None } else { Some(1) };
                buf.push_str(&self.render_qualified_identifier(
                    table.as_deref(),
                    column,
                    max_depth,
                )?);
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
            Expression::Case {
                operand,
                when_thens,
                else_result,
            } => {
                buf.push_str("CASE");
                if let Some(op) = operand {
                    buf.push(' ');
                    self.render_expression_to(op, buf)?;
                }
                for wt in when_thens.iter() {
                    buf.push_str(" WHEN ");
                    self.render_expression_to(&wt.when, buf)?;
                    buf.push_str(" THEN ");
                    self.render_expression_to(&wt.then, buf)?;
                }
                if let Some(el) = else_result {
                    buf.push_str(" ELSE ");
                    self.render_expression_to(el, buf)?;
                }
                buf.push_str(" END");
                Ok(())
            }
            Expression::WindowFunction {
                name,
                args,
                distinct,
                over,
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
                self.render_window_spec(over, buf)?;
                Ok(())
            }
        }
    }

    pub(crate) fn render_cte_cast(
        &mut self,
        placeholder: &str,
        data_type: DataType,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match self.dialect {
            SqlDialect::Postgres => {
                let cast_type = match data_type {
                    DataType::Int => "INTEGER",
                    DataType::Float => "DOUBLE PRECISION",
                    DataType::Boolean => "BOOLEAN",
                    DataType::Decimal => "NUMERIC",
                    DataType::Date => "DATE",
                    DataType::Timestamp => "TIMESTAMP",
                    DataType::Json => "JSON",
                    _ => return write!(buf, "{placeholder}").map_err(formatting_error),
                };
                write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
            }
            SqlDialect::MySql => {
                let cast_type = match data_type {
                    DataType::Int => "SIGNED",
                    DataType::Float => "DECIMAL(65, 30)",
                    DataType::Boolean => "UNSIGNED",
                    DataType::Decimal => "DECIMAL(65, 10)",
                    DataType::Date => "DATE",
                    DataType::Timestamp => "DATETIME",
                    DataType::Json => "JSON",
                    DataType::String => "CHAR",
                    _ => return write!(buf, "{placeholder}").map_err(formatting_error),
                };
                write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
            }
            SqlDialect::Sqlite => {
                let cast_type = match data_type {
                    DataType::Int => "INTEGER",
                    DataType::Float => "REAL",
                    DataType::Boolean => "INTEGER",
                    DataType::Decimal => "REAL",
                    DataType::Date => "TEXT",
                    DataType::Timestamp => "TEXT",
                    DataType::String => "TEXT",
                    DataType::Json => "TEXT",
                    DataType::Uuid => "TEXT",
                    DataType::Null => "INTEGER",
                    _ => return write!(buf, "{placeholder}").map_err(formatting_error),
                };
                write!(buf, "CAST({placeholder} AS {cast_type})").map_err(formatting_error)
            }
        }
    }

    pub(crate) fn render_predicate_to(
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
            Predicate::True => {
                buf.push_str("TRUE");
                Ok(())
            }
            Predicate::False => {
                buf.push_str("FALSE");
                Ok(())
            }
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
            let mut chars = segment.chars();
            let valid = chars
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
                && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
            if !valid {
                return Err(compilation_error(
                    "invalid_function_name",
                    json!({"function": function}),
                ));
            }
        }
        Ok(Cow::Borrowed(function))
    }

    fn render_window_spec(
        &mut self,
        spec: &WindowSpec,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        buf.push_str(" OVER (");
        let mut clause_added = false;

        if let Some(partition_by) = &spec.partition_by {
            clause_added = true;
            buf.push_str("PARTITION BY ");
            for (i, expr) in partition_by.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                self.render_expression_to(expr, buf)?;
            }
        }

        if let Some(order_by) = &spec.order_by {
            if clause_added {
                buf.push(' ');
            }
            clause_added = true;
            buf.push_str("ORDER BY ");
            for (i, term) in order_by.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                self.render_expression_to(&term.expr, buf)?;
                if term.descending {
                    buf.push_str(" DESC");
                } else {
                    buf.push_str(" ASC");
                }
            }
        }

        if let Some(frame) = &spec.frame {
            if clause_added {
                buf.push(' ');
            }
            self.render_window_frame(frame, buf)?;
        }

        buf.push(')');
        Ok(())
    }

    fn render_window_frame(
        &mut self,
        frame: &WindowFrame,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match frame.kind {
            WindowFrameKind::Rows => buf.push_str("ROWS"),
            WindowFrameKind::Range => buf.push_str("RANGE"),
            WindowFrameKind::Groups => buf.push_str("GROUPS"),
        }
        buf.push_str(" BETWEEN ");
        self.render_window_frame_bound(&frame.start, buf)?;
        buf.push_str(" AND ");
        match &frame.end {
            Some(end) => self.render_window_frame_bound(end, buf)?,
            None => buf.push_str("CURRENT ROW"),
        }
        Ok(())
    }

    fn render_window_frame_bound(
        &mut self,
        bound: &WindowFrameBound,
        buf: &mut String,
    ) -> Result<(), VlorQLError> {
        match bound {
            WindowFrameBound::UnboundedPreceding => buf.push_str("UNBOUNDED PRECEDING"),
            WindowFrameBound::Preceding(expr) => {
                self.render_expression_to(expr, buf)?;
                buf.push_str(" PRECEDING");
            }
            WindowFrameBound::CurrentRow => buf.push_str("CURRENT ROW"),
            WindowFrameBound::Following(expr) => {
                self.render_expression_to(expr, buf)?;
                buf.push_str(" FOLLOWING");
            }
            WindowFrameBound::UnboundedFollowing => buf.push_str("UNBOUNDED FOLLOWING"),
        }
        Ok(())
    }

    fn render_binary_operator(&self, operator: BinaryOperator) -> Cow<'static, str> {
        match operator {
            BinaryOperator::Add => Cow::Borrowed("+"),
            BinaryOperator::Sub => Cow::Borrowed("-"),
            BinaryOperator::Mul => Cow::Borrowed("*"),
            BinaryOperator::Div => Cow::Borrowed("/"),
            BinaryOperator::Mod => Cow::Borrowed("%"),
            BinaryOperator::And => Cow::Borrowed("AND"),
            BinaryOperator::Or => Cow::Borrowed("OR"),
            BinaryOperator::Eq => Cow::Borrowed("="),
            BinaryOperator::Neq => Cow::Borrowed("<>"),
            BinaryOperator::Gt => Cow::Borrowed(">"),
            BinaryOperator::Lt => Cow::Borrowed("<"),
            BinaryOperator::Gte => Cow::Borrowed(">="),
            BinaryOperator::Lte => Cow::Borrowed("<="),
            BinaryOperator::Like => Cow::Borrowed("LIKE"),
            BinaryOperator::ILike => {
                if self.dialect == SqlDialect::Postgres
                    && !self.config.type_mappings.contains_key("ilike")
                {
                    Cow::Borrowed("ILIKE")
                } else {
                    Cow::Owned(
                        self.config
                            .type_mappings
                            .get("ilike")
                            .cloned()
                            .unwrap_or_else(|| "LIKE".to_owned()),
                    )
                }
            }
        }
    }

    fn render_comparison_operator(
        &self,
        operator: ComparisonOperator,
    ) -> Result<Cow<'static, str>, VlorQLError> {
        match operator {
            ComparisonOperator::Eq => Ok(Cow::Borrowed("=")),
            ComparisonOperator::Neq => Ok(Cow::Borrowed("<>")),
            ComparisonOperator::Gt => Ok(Cow::Borrowed(">")),
            ComparisonOperator::Lt => Ok(Cow::Borrowed("<")),
            ComparisonOperator::Gte => Ok(Cow::Borrowed(">=")),
            ComparisonOperator::Lte => Ok(Cow::Borrowed("<=")),
            ComparisonOperator::Like => Ok(Cow::Borrowed("LIKE")),
            ComparisonOperator::ILike => {
                if self.dialect == SqlDialect::Postgres
                    && !self.config.type_mappings.contains_key("ilike")
                {
                    Ok(Cow::Borrowed("ILIKE"))
                } else {
                    Ok(Cow::Owned(
                        self.config
                            .type_mappings
                            .get("ilike")
                            .cloned()
                            .unwrap_or_else(|| "LIKE".to_owned()),
                    ))
                }
            }
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
}
