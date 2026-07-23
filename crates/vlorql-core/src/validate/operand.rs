//! Recursive expression and predicate type validation.

use crate::errors::{ValidationErrorKind, VlorQLError};
use crate::query::QuerySource;
use crate::schema::{
    BinaryOperator, ComparisonOperator, DataType, Expression, InTarget, Predicate, Projection,
    QueryPlan, SchemaSnapshot, WindowFrameBound,
};
use serde_json::{Value, json};
use std::collections::HashSet;

/// Validates expression operand types against a schema snapshot.
pub struct OperandValidator<'a> {
    schema: &'a SchemaSnapshot,
}

impl<'a> OperandValidator<'a> {
    /// Creates an operand validator borrowing a schema snapshot.
    pub fn new(schema: &'a SchemaSnapshot) -> Self {
        Self { schema }
    }

    /// Convenience entry point equivalent to `OperandValidator::new(schema).validate_plan(plan)`.
    pub fn validate(plan: &QueryPlan, schema: &'a SchemaSnapshot) -> Result<(), Vec<VlorQLError>> {
        Self::new(schema).validate_plan(plan)
    }

    /// Validates every expression and predicate in a plan, including nested CTEs.
    pub fn validate_plan(&self, plan: &QueryPlan) -> Result<(), Vec<VlorQLError>> {
        let mut errors = Vec::new();
        self.validate_plan_inner(plan, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Resolves and validates one expression outside a query-plan context.
    pub fn validate_expression(
        &self,
        expression: &Expression,
    ) -> Result<DataType, Vec<VlorQLError>> {
        let scope = OperandScope::from_schema(self.schema);
        let mut errors = Vec::new();
        let data_type = self
            .validate_expression_inner(expression, &scope, &mut errors)
            .unwrap_or(DataType::Null);
        if errors.is_empty() {
            Ok(data_type)
        } else {
            Err(errors)
        }
    }

    /// Validates one predicate outside a query-plan context.
    pub fn validate_predicate(&self, predicate: &Predicate) -> Result<(), Vec<VlorQLError>> {
        let scope = OperandScope::from_schema(self.schema);
        let mut errors = Vec::new();
        self.validate_predicate_inner(predicate, &scope, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_plan_inner(&self, plan: &QueryPlan, errors: &mut Vec<VlorQLError>) {
        if let Some(ctes) = &plan.ctes {
            for cte in ctes {
                self.validate_plan_inner(&cte.query, errors);
            }
        }

        let scope = OperandScope::from_plan(plan, self.schema);
        for projection in &plan.select {
            if let Projection::Expr { expression, .. } = projection {
                self.validate_expression_inner(expression, &scope, errors);
            }
        }
        if let Some(predicate) = &plan.r#where {
            self.validate_predicate_inner(predicate, &scope, errors);
        }
        if let Some(expressions) = &plan.group_by {
            for expression in expressions {
                self.validate_expression_inner(expression, &scope, errors);
            }
        }
        if let Some(predicate) = &plan.having {
            self.validate_predicate_inner(predicate, &scope, errors);
        }
        if let Some(terms) = &plan.order_by {
            for term in terms {
                self.validate_expression_inner(&term.expr, &scope, errors);
            }
        }
        if let Some(joins) = &plan.joins {
            for join in joins {
                self.validate_predicate_inner(&join.on, &scope, errors);
            }
        }
    }

    fn validate_expression_inner(
        &self,
        expression: &Expression,
        scope: &OperandScope,
        errors: &mut Vec<VlorQLError>,
    ) -> Option<DataType> {
        match expression {
            Expression::Literal { value, data_type } => {
                if !literal_matches_type(value, *data_type) {
                    // Small local LLMs frequently set data_type to Null with a non-null
                    // value (e.g. {"value":"done","data_type":"null"}).  Infer the
                    // correct data type from the JSON value and treat it as a warning-free
                    // auto-fix so the plan can proceed.
                    if *data_type == DataType::Null && !value.is_null() {
                        let inferred = match value {
                            Value::String(_) => DataType::String,
                            Value::Bool(_) => DataType::Boolean,
                            Value::Number(n) if n.is_f64() => DataType::Float,
                            Value::Number(_) => DataType::Int,
                            _ => DataType::Null,
                        };
                        tracing::debug!(
                            "Literal with declared data_type=null and non-null value; inferred {inferred:?}"
                        );
                        return Some(inferred);
                    }
                    errors.push(type_mismatch_error(
                        data_type_name(*data_type),
                        json_value_type(value),
                        "literal",
                        json!({"value": value, "declared_type": data_type}),
                    ));
                }
                Some(*data_type)
            }
            Expression::ColumnRef { table, column } => {
                scope.resolve_column_type(table.as_deref(), column, self.schema)
            }
            Expression::FunctionCall { name, args, .. } => {
                let argument_types = args
                    .iter()
                    .map(|argument| self.validate_expression_inner(argument, scope, errors))
                    .collect::<Vec<_>>();
                Some(self.validate_function(name, &argument_types, errors))
            }
            Expression::BinaryOp { left, op, right } => {
                let left_type = self.validate_expression_inner(left, scope, errors);
                let right_type = self.validate_expression_inner(right, scope, errors);
                match (left_type, right_type) {
                    (Some(left_type), Some(right_type)) => Some(validate_binary_operation(
                        *op, left_type, right_type, errors,
                    )),
                    _ => None,
                }
            }
            Expression::Star => None,
            Expression::SubQuery { query } => {
                self.validate_plan_inner(query, errors);
                None
            }
            Expression::Case {
                operand,
                when_thens,
                else_result,
            } => {
                if let Some(op) = operand {
                    self.validate_expression_inner(op, scope, errors);
                }
                for wt in when_thens {
                    self.validate_expression_inner(&wt.when, scope, errors);
                    self.validate_expression_inner(&wt.then, scope, errors);
                }
                if let Some(el) = else_result {
                    self.validate_expression_inner(el, scope, errors);
                }
                // CASE type = common type of all THEN and ELSE branches
                let then_types: Vec<DataType> = when_thens
                    .iter()
                    .filter_map(|wt| {
                        self.validate_expression_inner(&wt.then, scope, errors)
                    })
                    .collect();
                let else_type = else_result
                    .as_ref()
                    .and_then(|el| self.validate_expression_inner(el, scope, errors));
                then_types
                    .into_iter()
                    .chain(else_type)
                    .reduce(|a, b| if a == b { a } else { DataType::String })
            }
            Expression::WindowFunction {
                name, args, over, ..
            } => {
                let argument_types = args
                    .iter()
                    .map(|argument| self.validate_expression_inner(argument, scope, errors))
                    .collect::<Vec<_>>();
                // Validate expressions inside the window spec.
                if let Some(partition_by) = &over.partition_by {
                    for expr in partition_by {
                        self.validate_expression_inner(expr, scope, errors);
                    }
                }
                if let Some(order_by) = &over.order_by {
                    for term in order_by {
                        self.validate_expression_inner(&term.expr, scope, errors);
                    }
                }
                if let Some(frame) = &over.frame {
                    if let Some(start) = frame_bound_expr(&frame.start) {
                        self.validate_expression_inner(start, scope, errors);
                    }
                    if let Some(end) = &frame.end {
                        if let Some(expr) = frame_bound_expr(end) {
                            self.validate_expression_inner(expr, scope, errors);
                        }
                    }
                }
                Some(self.validate_function(name, &argument_types, errors))
            }
        }
    }

    fn validate_predicate_inner(
        &self,
        predicate: &Predicate,
        scope: &OperandScope,
        errors: &mut Vec<VlorQLError>,
    ) {
        match predicate {
            Predicate::Comparison { left, op, right } => {
                let left_type = self.validate_expression_inner(left, scope, errors);
                let right_type = self.validate_expression_inner(right, scope, errors);
                if let (Some(left_type), Some(right_type)) = (left_type, right_type) {
                    validate_comparison(*op, left_type, right_type, errors);
                }
            }
            Predicate::And { left, right } | Predicate::Or { left, right } => {
                self.validate_predicate_inner(left, scope, errors);
                self.validate_predicate_inner(right, scope, errors);
            }
            Predicate::Not { child } => self.validate_predicate_inner(child, scope, errors),
            Predicate::Between { expr, low, high } => {
                let expr_type = self.validate_expression_inner(expr, scope, errors);
                let low_type = self.validate_expression_inner(low, scope, errors);
                let high_type = self.validate_expression_inner(high, scope, errors);
                if let (Some(expr_type), Some(low_type)) = (expr_type, low_type) {
                    validate_compatible_types("BETWEEN lower bound", expr_type, low_type, errors);
                }
                if let (Some(expr_type), Some(high_type)) = (expr_type, high_type) {
                    validate_compatible_types("BETWEEN upper bound", expr_type, high_type, errors);
                }
            }
            Predicate::In { expr, target } => {
                let expr_type = self.validate_expression_inner(expr, scope, errors);
                match target {
                    InTarget::Values(values) => {
                        for value in values {
                            let value_type = self.validate_expression_inner(value, scope, errors);
                            if let (Some(expr_type), Some(value_type)) = (expr_type, value_type) {
                                validate_compatible_types(
                                    "IN value", expr_type, value_type, errors,
                                );
                            }
                        }
                    }
                    InTarget::SubQuery(query) => {
                        self.validate_plan_inner(query, errors);
                    }
                }
            }
            Predicate::Exists { query } => {
                self.validate_plan_inner(query, errors);
            }
            Predicate::Like { expr, .. } => {
                if let Some(data_type) = self.validate_expression_inner(expr, scope, errors) {
                    require_string("LIKE expression", data_type, errors);
                }
            }
            Predicate::IsNull { expr } => {
                self.validate_expression_inner(expr, scope, errors);
            }
        }
    }

    fn validate_function(
        &self,
        name: &str,
        argument_types: &[Option<DataType>],
        errors: &mut Vec<VlorQLError>,
    ) -> DataType {
        // Match case-insensitively without allocating a lowercased copy.
        if name.eq_ignore_ascii_case("count") {
            require_arity_range(name, argument_types.len(), 0, 1, errors);
            DataType::Int
        } else if is_any_of_ignore_case(name, &["sum", "avg", "abs"]) {
            require_arity(name, argument_types.len(), 1, errors);
            if let Some(Some(argument_type)) = argument_types.first() {
                require_numeric(
                    &format!("function `{name}` argument"),
                    *argument_type,
                    errors,
                );
            }
            if name.eq_ignore_ascii_case("avg") {
                DataType::Float
            } else {
                argument_types
                    .first()
                    .copied()
                    .flatten()
                    .unwrap_or(DataType::Null)
            }
        } else if is_any_of_ignore_case(name, &["min", "max"]) {
            require_arity(name, argument_types.len(), 1, errors);
            argument_types
                .first()
                .copied()
                .flatten()
                .unwrap_or(DataType::Null)
        } else if is_any_of_ignore_case(name, &["lower", "upper"]) {
            require_arity(name, argument_types.len(), 1, errors);
            if let Some(Some(argument_type)) = argument_types.first() {
                require_string(
                    &format!("function `{name}` argument"),
                    *argument_type,
                    errors,
                );
            }
            DataType::String
        } else if name.eq_ignore_ascii_case("length") {
            require_arity(name, argument_types.len(), 1, errors);
            if let Some(Some(argument_type)) = argument_types.first() {
                require_string(
                    &format!("function `{name}` argument"),
                    *argument_type,
                    errors,
                );
            }
            DataType::Int
        } else if name.eq_ignore_ascii_case("concat") {
            require_min_arity(name, argument_types.len(), 1, errors);
            for argument_type in argument_types.iter().flatten() {
                require_string(
                    &format!("function `{name}` argument"),
                    *argument_type,
                    errors,
                );
            }
            DataType::String
        } else if name.eq_ignore_ascii_case("coalesce") {
            require_min_arity(name, argument_types.len(), 1, errors);
            let result = argument_types
                .iter()
                .flatten()
                .copied()
                .find(|data_type| *data_type != DataType::Null)
                .unwrap_or(DataType::Null);
            for argument_type in argument_types.iter().flatten() {
                validate_compatible_types(
                    &format!("function `{name}` argument"),
                    result,
                    *argument_type,
                    errors,
                );
            }
            result
        } else {
            DataType::Null
        }
    }
}

#[derive(Debug)]
struct OperandScope {
    sources: Vec<QuerySource>,
    cte_names: HashSet<String>,
}

impl OperandScope {
    fn from_plan(plan: &QueryPlan, schema: &SchemaSnapshot) -> Self {
        let scope = crate::query::QueryScope::from_plan(plan);
        let mut sources = scope.sources;
        sources.retain(|source| {
            scope.cte_names.contains(&source.table) || schema.get_table(&source.table).is_some()
        });
        Self {
            sources,
            cte_names: scope.cte_names,
        }
    }

    fn from_schema(schema: &SchemaSnapshot) -> Self {
        Self {
            sources: schema
                .tables
                .iter()
                .map(|table| QuerySource {
                    table: table.name.clone(),
                    alias: None,
                })
                .collect(),
            cte_names: HashSet::new(),
        }
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        column: &str,
        schema: &SchemaSnapshot,
    ) -> Option<DataType> {
        if let Some(qualifier) = qualifier {
            let source = self.sources.iter().find(|source| {
                source.table == qualifier || source.alias.as_deref() == Some(qualifier)
            })?;
            if self.cte_names.contains(&source.table) {
                return None;
            }
            return schema
                .get_column(&source.table, column)
                .map(|column| column.data_type);
        }

        self.sources
            .iter()
            .filter(|source| !self.cte_names.contains(&source.table))
            .filter_map(|source| schema.get_column(&source.table, column))
            .map(|column| column.data_type)
            .next()
    }
}

fn validate_binary_operation(
    operator: BinaryOperator,
    left: DataType,
    right: DataType,
    errors: &mut Vec<VlorQLError>,
) -> DataType {
    match operator {
        BinaryOperator::Add
        | BinaryOperator::Sub
        | BinaryOperator::Mul
        | BinaryOperator::Div
        | BinaryOperator::Mod => {
            if !are_numeric(left, right) {
                errors.push(type_mismatch_error(
                    "compatible numeric operands",
                    format!("{} and {}", data_type_name(left), data_type_name(right)),
                    format!("binary operator {operator:?}"),
                    json!({"left": left, "right": right, "operator": operator}),
                ));
            }
            numeric_result_type(left, right)
        }
        BinaryOperator::And | BinaryOperator::Or => {
            if left != DataType::Boolean || right != DataType::Boolean {
                errors.push(type_mismatch_error(
                    "boolean operands",
                    format!("{} and {}", data_type_name(left), data_type_name(right)),
                    format!("binary operator {operator:?}"),
                    json!({"left": left, "right": right, "operator": operator}),
                ));
            }
            DataType::Boolean
        }
        BinaryOperator::Eq
        | BinaryOperator::Neq
        | BinaryOperator::Gt
        | BinaryOperator::Lt
        | BinaryOperator::Gte
        | BinaryOperator::Lte => {
            validate_compatible_types(
                &format!("binary operator {operator:?}"),
                left,
                right,
                errors,
            );
            DataType::Boolean
        }
        BinaryOperator::Like | BinaryOperator::ILike => {
            if !is_string_compatible(left) || !is_string_compatible(right) {
                errors.push(type_mismatch_error(
                    "string operands",
                    format!("{} and {}", data_type_name(left), data_type_name(right)),
                    format!("binary operator {operator:?}"),
                    json!({"left": left, "right": right, "operator": operator}),
                ));
            }
            DataType::Boolean
        }
    }
}

fn validate_comparison(
    operator: ComparisonOperator,
    left: DataType,
    right: DataType,
    errors: &mut Vec<VlorQLError>,
) {
    match operator {
        ComparisonOperator::Like | ComparisonOperator::ILike => {
            if !is_string_compatible(left) || !is_string_compatible(right) {
                errors.push(type_mismatch_error(
                    "string operands",
                    format!("{} and {}", data_type_name(left), data_type_name(right)),
                    format!("comparison {operator:?}"),
                    json!({"left": left, "right": right, "operator": operator}),
                ));
            }
        }
        _ => validate_compatible_types(&format!("comparison {operator:?}"), left, right, errors),
    }
}

fn validate_compatible_types(
    expression: &str,
    left: DataType,
    right: DataType,
    errors: &mut Vec<VlorQLError>,
) {
    if !types_compatible(left, right) {
        errors.push(type_mismatch_error(
            data_type_name(left),
            data_type_name(right),
            expression,
            json!({"left": left, "right": right}),
        ));
    }
}

fn require_numeric(expression: &str, actual: DataType, errors: &mut Vec<VlorQLError>) {
    if !is_numeric(actual) && actual != DataType::Null {
        errors.push(type_mismatch_error(
            "numeric",
            data_type_name(actual),
            expression,
            json!({"actual": actual}),
        ));
    }
}

fn require_string(expression: &str, actual: DataType, errors: &mut Vec<VlorQLError>) {
    if !is_string_compatible(actual) {
        errors.push(type_mismatch_error(
            "string",
            data_type_name(actual),
            expression,
            json!({"actual": actual}),
        ));
    }
}

fn require_arity(name: &str, actual: usize, expected: usize, errors: &mut Vec<VlorQLError>) {
    if actual != expected {
        errors.push(type_mismatch_error(
            format!("{expected} argument(s)"),
            format!("{actual} argument(s)"),
            format!("function `{name}`"),
            json!({"function": name, "expected_arguments": expected, "actual_arguments": actual}),
        ));
    }
}

fn require_min_arity(name: &str, actual: usize, min: usize, errors: &mut Vec<VlorQLError>) {
    if actual < min {
        errors.push(type_mismatch_error(
            format!("at least {min} argument(s)"),
            format!("{actual} argument(s)"),
            format!("function `{name}`"),
            json!({"function": name, "minimum_arguments": min, "actual_arguments": actual}),
        ));
    }
}

fn require_arity_range(
    name: &str,
    actual: usize,
    min: usize,
    max: usize,
    errors: &mut Vec<VlorQLError>,
) {
    if !(min..=max).contains(&actual) {
        errors.push(type_mismatch_error(
            format!("between {min} and {max} argument(s)"),
            format!("{actual} argument(s)"),
            format!("function `{name}`"),
            json!({
                "function": name,
                "minimum_arguments": min,
                "maximum_arguments": max,
                "actual_arguments": actual,
            }),
        ));
    }
}

fn type_mismatch_error(
    expected: impl Into<String>,
    found: impl Into<String>,
    expression: impl Into<String>,
    details: Value,
) -> VlorQLError {
    VlorQLError::validation(
        ValidationErrorKind::TypeMismatch {
            expected: expected.into(),
            found: found.into(),
            expr: expression.into(),
        },
        details,
    )
}

fn literal_matches_type(value: &Value, data_type: DataType) -> bool {
    match data_type {
        DataType::Int => value.as_i64().is_some() || value.as_u64().is_some(),
        DataType::Float => value.is_number(),
        DataType::String | DataType::Date | DataType::Timestamp | DataType::Uuid => {
            value.is_string()
        }
        DataType::Boolean => value.is_boolean(),
        DataType::Json => true,
        DataType::Null => value.is_null(),
    }
}

fn json_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "float",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Int => "int",
        DataType::Float => "float",
        DataType::String => "string",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
        DataType::Timestamp => "timestamp",
        DataType::Json => "json",
        DataType::Null => "null",
        DataType::Uuid => "uuid",
    }
}

fn is_numeric(data_type: DataType) -> bool {
    matches!(data_type, DataType::Int | DataType::Float)
}

fn are_numeric(left: DataType, right: DataType) -> bool {
    (is_numeric(left) || left == DataType::Null) && (is_numeric(right) || right == DataType::Null)
}

fn is_string_compatible(data_type: DataType) -> bool {
    matches!(data_type, DataType::String | DataType::Null)
}

fn types_compatible(left: DataType, right: DataType) -> bool {
    left == right || left == DataType::Null || right == DataType::Null || are_numeric(left, right)
}

fn numeric_result_type(left: DataType, right: DataType) -> DataType {
    if left == DataType::Float || right == DataType::Float {
        DataType::Float
    } else if left == DataType::Int || right == DataType::Int {
        DataType::Int
    } else {
        DataType::Null
    }
}

/// Returns `true` when `name` matches any candidate case-insensitively.
fn is_any_of_ignore_case(name: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Extracts the inner expression from a window frame bound, if any.
fn frame_bound_expr(bound: &WindowFrameBound) -> Option<&Expression> {
    match bound {
        WindowFrameBound::Preceding(expr) | WindowFrameBound::Following(expr) => Some(expr),
        _ => None,
    }
}
