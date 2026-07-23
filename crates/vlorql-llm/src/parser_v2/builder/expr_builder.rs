//! Expression builder: canonical JSON → [`Expression`] / [`Predicate`].
//!
//! Builds typed AST nodes from canonical JSON.  This layer does **no**
//! repair — it assumes the input has already been normalized.

use serde_json::Value;
use std::fmt;
use vlorql_core::schema::{
    BinaryOperator, ComparisonOperator, DataType, Expression, InTarget, Predicate,
};

/// Error returned when building an AST node from JSON fails.
#[derive(Debug, Clone)]
pub struct BuildError {
    path: String,
    message: String,
}

impl BuildError {
    /// Create a new error at the current path.
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Prepend a field name to the error path.
    pub fn at(self, field: &str) -> Self {
        let new_path = if self.path.is_empty() {
            field.to_owned()
        } else {
            format!("{}.{}", field, self.path)
        };
        Self {
            path: new_path,
            message: self.message,
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "at `{}`: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for BuildError {}

impl From<BuildError> for serde_json::Error {
    fn from(e: BuildError) -> Self {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    }
}

// ── Field extraction helpers ──────────────────────────────────────

/// Extract a required string field from a JSON object.
pub fn req_str<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a str, BuildError> {
    obj.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
        let actual = obj.get(key).map(type_name).unwrap_or("(missing)");
        BuildError::new(path, format!("expected string `{key}`, got {actual}"))
    })
}

/// Extract an optional string field from a JSON object.
pub fn opt_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(|v| v.as_str())
}

/// Extract a required object from a JSON value.
pub fn req_obj<'a>(
    val: &'a Value,
    parent: &str,
) -> Result<&'a serde_json::Map<String, Value>, BuildError> {
    val.as_object()
        .ok_or_else(|| BuildError::new(parent, format!("expected object, got {}", type_name(val))))
}

/// Extract a required array from a JSON value.
pub fn req_arr<'a>(val: &'a Value, parent: &str) -> Result<&'a [Value], BuildError> {
    val.as_array()
        .map(|v| v.as_slice())
        .ok_or_else(|| BuildError::new(parent, format!("expected array, got {}", type_name(val))))
}

/// Human-readable type name for a JSON value.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ── Operator / type parsers ───────────────────────────────────────

/// Parse a comparison operator string.
pub fn parse_comparison_op(s: &str) -> Result<ComparisonOperator, BuildError> {
    use ComparisonOperator::*;
    match s {
        "eq" => Ok(Eq),
        "ne" => Ok(Neq),
        "gt" => Ok(Gt),
        "gte" => Ok(Gte),
        "lt" => Ok(Lt),
        "lte" => Ok(Lte),
        _ => Err(BuildError::new(
            "op",
            format!("unknown comparison operator `{s}`"),
        )),
    }
}

/// Parse a binary operator string.
pub fn parse_binary_op(s: &str) -> Result<BinaryOperator, BuildError> {
    use BinaryOperator::*;
    match s {
        "add" => Ok(Add),
        "sub" => Ok(Sub),
        "mul" => Ok(Mul),
        "div" => Ok(Div),
        "mod" => Ok(Mod),
        _ => Err(BuildError::new(
            "op",
            format!("unknown binary operator `{s}`"),
        )),
    }
}

/// Parse a join type string.
pub fn parse_join_type(s: &str) -> Result<vlorql_core::schema::JoinType, BuildError> {
    use vlorql_core::schema::JoinType::*;
    match s {
        "inner" => Ok(Inner),
        "left" => Ok(Left),
        "right" => Ok(Right),
        "full" => Ok(Full),
        "cross" => Ok(Cross),
        _ => Err(BuildError::new(
            "join_type",
            format!("unknown join type `{s}`"),
        )),
    }
}

/// Parse a data type string.
pub fn parse_data_type(s: &str) -> Result<DataType, BuildError> {
    use DataType::*;
    match s {
        "int" => Ok(Int),
        "string" => Ok(String),
        "float" => Ok(Float),
        "boolean" => Ok(Boolean),
        "timestamp" => Ok(Timestamp),
        "null" => Ok(Null),
        "json" => Ok(Json),
        "uuid" => Ok(Uuid),
        other => Err(BuildError::new(
            "data_type",
            format!("unknown data type `{other}`"),
        )),
    }
}

// ── Expression builder ────────────────────────────────────────────

/// Build a literal expression from a bare JSON value (number, string, bool, null).
fn build_literal_expr(val: &Value) -> Result<Expression, BuildError> {
    match val {
        Value::Null => Ok(Expression::Literal {
            value: Value::Null,
            data_type: DataType::Null,
        }),
        Value::Bool(b) => Ok(Expression::Literal {
            value: Value::Bool(*b),
            data_type: DataType::Boolean,
        }),
        Value::Number(n) => {
            let dt = if n.is_f64() {
                DataType::Float
            } else {
                DataType::Int
            };
            Ok(Expression::Literal {
                value: val.clone(),
                data_type: dt,
            })
        }
        Value::String(s) => Ok(Expression::Literal {
            value: Value::String(s.clone()),
            data_type: DataType::String,
        }),
        _ => Err(BuildError::new(
            "",
            format!("cannot infer Expression from {}", type_name(val)),
        )),
    }
}

/// Build a literal expression from a JSON object with `value` and `data_type`.
fn build_literal_from_obj(obj: &serde_json::Map<String, Value>) -> Result<Expression, BuildError> {
    let value = obj.get("value").cloned().unwrap_or(Value::Null);
    let dt_str = obj.get("data_type").and_then(|v| v.as_str()).map(|s| s.to_owned());
    let data_type = match dt_str.as_deref() {
        Some("null") => {
            // Small local LLMs (llama3.2, etc.) frequently set data_type to "null"
            // while providing a non-null value.  Infer the correct type from the value.
            match &value {
                Value::Null => DataType::Null,
                Value::Bool(_) => DataType::Boolean,
                Value::Number(n) if n.is_f64() => DataType::Float,
                Value::Number(_) => DataType::Int,
                Value::String(_) => DataType::String,
                _ => DataType::Null,
            }
        }
        Some(dt) => parse_data_type(dt)?,
        None => DataType::Null,
    };
    Ok(Expression::Literal { value, data_type })
}

/// Build an [`Expression`] from a canonical JSON value.
///
/// Accepts both objects (with `type` discriminator) and bare values
/// (numbers, strings, booleans, nulls) which are inferred as literals.
pub fn build_expression(val: &Value) -> Result<Expression, BuildError> {
    let obj = match val.as_object() {
        Some(o) => o,
        None => {
            // Bare value — infer as literal.
            return build_literal_expr(val);
        }
    };

    let type_str = match obj.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => {
            // Canonical input should always have type.  Fall back to
            // inference for robustness.
            if obj.contains_key("column") {
                "column_ref"
            } else if obj.contains_key("value") {
                "literal"
            } else if obj.contains_key("name") && obj.contains_key("args") {
                "function_call"
            } else if obj.contains_key("query") {
                "subquery"
            } else {
                return Err(BuildError::new(
                    "type",
                    format!(
                        "missing `type` discriminator on Expression (keys: {:?})",
                        obj.keys().collect::<Vec<_>>()
                    ),
                ));
            }
        }
    };

    match type_str {
        // `expr` is the `Projection` wrapper, only valid in `select`. LLMs
        // frequently emit it in bare Expression positions (order_by, having,
        // comparison operands, group_by). Unwrap the inner expression so the
        // plan still builds instead of failing on an "unknown variant".
        "expr" | "Expr" => {
            let inner = obj
                .get("expression")
                .or_else(|| obj.get("expr"))
                .ok_or_else(|| {
                    BuildError::new("expression", "`expr` wrapper missing `expression` field")
                })?;
            build_expression(inner).map_err(|e| e.at("expression"))
        }
        "column_ref" => {
            let column = req_str(obj, "column", "")?.to_owned();
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            Ok(Expression::ColumnRef { table, column })
        }
        "literal" => build_literal_from_obj(obj),
        "function_call" => {
            let name = req_str(obj, "name", "")?.to_owned();
            let args_arr = req_arr(
                obj.get("args")
                    .ok_or_else(|| BuildError::new("", "missing `args` field on function_call"))?,
                "args",
            )?;
            let args = args_arr
                .iter()
                .enumerate()
                .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("args[{i}]"))))
                .collect::<Result<Vec<_>, _>>()?;
            let distinct = obj
                .get("distinct")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(Expression::FunctionCall {
                name,
                args,
                distinct,
            })
        }
        "binary_op" => {
            let left_val = obj
                .get("left")
                .ok_or_else(|| BuildError::new("", "missing `left` field on binary_op"))?;
            let left = build_expression(left_val).map_err(|e| e.at("left"))?;
            let op_str = req_str(obj, "op", "")?;
            let op = parse_binary_op(op_str)?;
            let right_val = obj
                .get("right")
                .ok_or_else(|| BuildError::new("", "missing `right` field on binary_op"))?;
            let right = build_expression(right_val).map_err(|e| e.at("right"))?;
            Ok(Expression::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        }
        "star" => Ok(Expression::Star),
        "subquery" => {
            let sub = req_obj(
                obj.get("query")
                    .ok_or_else(|| BuildError::new("", "missing `query` field on subquery"))?,
                "query",
            )?;
            let query = super::query_builder::build_plan_from_obj(sub)?;
            Ok(Expression::SubQuery {
                query: Box::new(query),
            })
        }
        _ => {
            // Fallback: if type_str is a known aggregate function name,
            // treat it as a FunctionCall.  The LLM sometimes emits
            // {"type": "sum", "args": [...]} instead of the canonical
            // {"type": "function_call", "name": "sum", ...}.
            const AGGREGATES: &[&str] = &[
                "sum",
                "count",
                "avg",
                "min",
                "max",
                "string_agg",
                "array_agg",
            ];
            if AGGREGATES.contains(&type_str) {
                let args_arr = req_arr(
                    obj.get("args").ok_or_else(|| {
                        BuildError::new(
                            "args",
                            format!("aggregate '{}' missing `args` field", type_str),
                        )
                    })?,
                    "args",
                )?;
                let args = args_arr
                    .iter()
                    .enumerate()
                    .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("args[{i}]"))))
                    .collect::<Result<Vec<_>, _>>()?;
                let distinct = obj
                    .get("distinct")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(Expression::FunctionCall {
                    name: type_str.to_owned(),
                    args,
                    distinct,
                })
            } else {
                Err(BuildError::new(
                    "type",
                    format!("unknown Expression variant `{type_str}`"),
                ))
            }
        }
    }
}

// ── Predicate builder ─────────────────────────────────────────────

/// Build a [`Predicate`] from a canonical JSON value.
pub fn build_predicate(val: &Value) -> Result<Predicate, BuildError> {
    let obj = val.as_object().ok_or_else(|| {
        BuildError::new(
            "",
            format!("expected object for Predicate, got {}", type_name(val)),
        )
    })?;

    let type_str = match obj.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => {
            if obj.contains_key("left") && obj.contains_key("op") {
                "comparison"
            } else {
                return Err(BuildError::new(
                    "type",
                    format!(
                        "missing `type` discriminator on Predicate (keys: {:?})",
                        obj.keys().collect::<Vec<_>>()
                    ),
                ));
            }
        }
    };

    match type_str {
        "comparison" => {
            let left = build_expression(
                obj.get("left")
                    .ok_or_else(|| BuildError::new("left", "missing `left` field"))?,
            )
            .map_err(|e| e.at("left"))?;
            let op_str = req_str(obj, "op", "")?;
            let op = parse_comparison_op(op_str)?;
            let right = build_expression(
                obj.get("right")
                    .ok_or_else(|| BuildError::new("right", "missing `right` field"))?,
            )
            .map_err(|e| e.at("right"))?;
            Ok(Predicate::Comparison { left, op, right })
        }
        "and" => {
            let left = build_predicate(
                obj.get("left")
                    .ok_or_else(|| BuildError::new("left", "missing `left` field"))?,
            )
            .map_err(|e| e.at("left"))?;
            let right = build_predicate(
                obj.get("right")
                    .ok_or_else(|| BuildError::new("right", "missing `right` field"))?,
            )
            .map_err(|e| e.at("right"))?;
            Ok(Predicate::And {
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        "or" => {
            let left = build_predicate(
                obj.get("left")
                    .ok_or_else(|| BuildError::new("left", "missing `left` field"))?,
            )
            .map_err(|e| e.at("left"))?;
            let right = build_predicate(
                obj.get("right")
                    .ok_or_else(|| BuildError::new("right", "missing `right` field"))?,
            )
            .map_err(|e| e.at("right"))?;
            Ok(Predicate::Or {
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        "not" => {
            let child = build_predicate(
                obj.get("child")
                    .ok_or_else(|| BuildError::new("child", "missing `child` field"))?,
            )
            .map_err(|e| e.at("child"))?;
            Ok(Predicate::Not {
                child: Box::new(child),
            })
        }
        "between" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let low = build_expression(
                obj.get("low")
                    .ok_or_else(|| BuildError::new("low", "missing `low` field"))?,
            )
            .map_err(|e| e.at("low"))?;
            let high = build_expression(
                obj.get("high")
                    .ok_or_else(|| BuildError::new("high", "missing `high` field"))?,
            )
            .map_err(|e| e.at("high"))?;
            Ok(Predicate::Between { expr, low, high })
        }
        "in" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let target_val = obj
                .get("target")
                .ok_or_else(|| BuildError::new("target", "missing `target` field on in"))?;
            let target = if let Some(arr) = target_val.as_array() {
                let values = arr
                    .iter()
                    .enumerate()
                    .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("target[{i}]"))))
                    .collect::<Result<Vec<_>, _>>()?;
                InTarget::Values(values)
            } else if let Some(sub_obj) = target_val.as_object() {
                let query = super::query_builder::build_plan_from_obj(sub_obj)?;
                InTarget::SubQuery(Box::new(query))
            } else {
                return Err(BuildError::new(
                    "target",
                    format!(
                        "expected array or object for IN target, got {}",
                        type_name(target_val)
                    ),
                ));
            };
            Ok(Predicate::In { expr, target })
        }
        "like" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let pattern = req_str(obj, "pattern", "")?.to_owned();
            Ok(Predicate::Like { expr, pattern })
        }
        "is_null" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            Ok(Predicate::IsNull { expr })
        }
        "exists" => {
            let sub = req_obj(
                obj.get("query")
                    .ok_or_else(|| BuildError::new("query", "missing `query` field on exists"))?,
                "query",
            )?;
            let query = super::query_builder::build_plan_from_obj(sub)?;
            Ok(Predicate::Exists {
                query: Box::new(query),
            })
        }
        other => Err(BuildError::new(
            "type",
            format!("unknown Predicate variant `{other}`"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vlorql_core::schema::ComparisonOperator::*;
    use vlorql_core::schema::DataType::*;

    // ── Expression building ───────────────────────────────────────

    #[test]
    fn build_column_ref() {
        let val = json!({"type": "column_ref", "table": "users", "column": "id"});
        let expr = build_expression(&val).unwrap();
        assert!(
            matches!(expr, Expression::ColumnRef { table: Some(t), column: c } if t == "users" && c == "id")
        );
    }

    #[test]
    fn build_column_ref_no_table() {
        let val = json!({"type": "column_ref", "column": "name"});
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::ColumnRef { table: None, column: c } if c == "name"));
    }

    #[test]
    fn build_literal_from_object() {
        let val = json!({"type": "literal", "value": 42, "data_type": "int"});
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::Literal { value: v, data_type: Int } if v == json!(42)));
    }

    #[test]
    fn build_literal_from_bare_number() {
        let val = json!(42);
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::Literal { value: v, data_type: Int } if v == json!(42)));
    }

    #[test]
    fn build_literal_from_bare_string() {
        let val = json!("hello");
        let expr = build_expression(&val).unwrap();
        assert!(
            matches!(expr, Expression::Literal { value: v, data_type: DataType::String } if v == json!("hello"))
        );
    }

    #[test]
    fn build_literal_from_bare_null() {
        let val = json!(null);
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::Literal { value: v, data_type: Null } if v.is_null()));
    }

    #[test]
    fn build_function_call() {
        let val = json!({"type": "function_call", "name": "count", "args": [{"type": "star"}], "distinct": false});
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::FunctionCall { name, .. } if name == "count"));
    }

    #[test]
    fn build_binary_op() {
        let val = json!({"type": "binary_op", "left": {"type": "column_ref", "column": "a"}, "op": "add", "right": {"type": "column_ref", "column": "b"}});
        let expr = build_expression(&val).unwrap();
        assert!(matches!(
            expr,
            Expression::BinaryOp {
                op: BinaryOperator::Add,
                ..
            }
        ));
    }

    #[test]
    fn build_star() {
        let val = json!({"type": "star"});
        let expr = build_expression(&val).unwrap();
        assert!(matches!(expr, Expression::Star));
    }

    #[test]
    fn build_infer_type_from_fields() {
        let val = json!({"column": "age", "table": "users"});
        let expr = build_expression(&val).unwrap();
        assert!(
            matches!(expr, Expression::ColumnRef { table: Some(t), column: c } if t == "users" && c == "age")
        );
    }

    // ── Predicate building ────────────────────────────────────────

    #[test]
    fn build_comparison() {
        let val = json!({"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Comparison { op: Gt, .. }));
    }

    #[test]
    fn build_and() {
        let val = json!({"type": "and", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}, "right": {"type": "comparison", "left": {"column": "b"}, "op": "gt", "right": {"value": 2}}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::And { .. }));
    }

    #[test]
    fn build_or() {
        let val = json!({"type": "or", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}, "right": {"type": "comparison", "left": {"column": "b"}, "op": "eq", "right": {"value": 2}}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Or { .. }));
    }

    #[test]
    fn build_not() {
        let val = json!({"type": "not", "child": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Not { .. }));
    }

    #[test]
    fn build_between() {
        let val = json!({"type": "between", "expr": {"column": "age"}, "low": {"value": 18}, "high": {"value": 65}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Between { .. }));
    }

    #[test]
    fn build_in_values() {
        let val = json!({"type": "in", "expr": {"column": "status"}, "target": [{"value": "active"}, {"value": "pending"}]});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(
            pred,
            Predicate::In {
                target: InTarget::Values(_),
                ..
            }
        ));
    }

    #[test]
    fn build_like() {
        let val = json!({"type": "like", "expr": {"column": "name"}, "pattern": "%john%"});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Like { pattern, .. } if pattern == "%john%"));
    }

    #[test]
    fn build_is_null() {
        let val = json!({"type": "is_null", "expr": {"column": "deleted_at"}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::IsNull { .. }));
    }

    #[test]
    fn build_exists() {
        let val = json!({"type": "exists", "query": {"select": [{"type": "star"}], "from": {"table": "users"}}});
        let pred = build_predicate(&val).unwrap();
        assert!(matches!(pred, Predicate::Exists { .. }));
    }

    // ── Error cases ───────────────────────────────────────────────

    #[test]
    fn error_on_missing_type() {
        let val = json!({"unknown": "field"});
        let result = build_expression(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_unknown_expression_type() {
        let val = json!({"type": "nonexistent"});
        let result = build_expression(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_unknown_predicate_type() {
        let val = json!({"type": "nonexistent"});
        let result = build_predicate(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_missing_op() {
        let val = json!({"type": "comparison", "left": {"column": "a"}, "right": {"value": 1}});
        let result = build_predicate(&val);
        assert!(result.is_err());
    }

    // ── Parser helpers ────────────────────────────────────────────

    #[test]
    fn parse_comparison_ops() {
        assert_eq!(parse_comparison_op("eq").unwrap(), Eq);
        assert_eq!(parse_comparison_op("ne").unwrap(), Neq);
        assert_eq!(parse_comparison_op("gt").unwrap(), Gt);
        assert_eq!(parse_comparison_op("gte").unwrap(), Gte);
        assert_eq!(parse_comparison_op("lt").unwrap(), Lt);
        assert_eq!(parse_comparison_op("lte").unwrap(), Lte);
        assert!(parse_comparison_op("unknown").is_err());
    }

    #[test]
    fn parse_binary_ops() {
        use BinaryOperator::*;
        assert_eq!(parse_binary_op("add").unwrap(), Add);
        assert_eq!(parse_binary_op("sub").unwrap(), Sub);
        assert_eq!(parse_binary_op("mul").unwrap(), Mul);
        assert_eq!(parse_binary_op("div").unwrap(), Div);
        assert_eq!(parse_binary_op("mod").unwrap(), Mod);
        assert!(parse_binary_op("unknown").is_err());
    }

    #[test]
    fn parse_data_types() {
        assert_eq!(parse_data_type("int").unwrap(), Int);
        assert_eq!(parse_data_type("string").unwrap(), DataType::String);
        assert_eq!(parse_data_type("float").unwrap(), Float);
        assert_eq!(parse_data_type("boolean").unwrap(), Boolean);
        assert_eq!(parse_data_type("timestamp").unwrap(), Timestamp);
        assert_eq!(parse_data_type("null").unwrap(), Null);
        assert!(parse_data_type("unknown").is_err());
    }

    #[test]
    fn parse_join_types() {
        use vlorql_core::schema::JoinType::*;
        assert_eq!(parse_join_type("inner").unwrap(), Inner);
        assert_eq!(parse_join_type("left").unwrap(), Left);
        assert_eq!(parse_join_type("right").unwrap(), Right);
        assert_eq!(parse_join_type("full").unwrap(), Full);
        assert_eq!(parse_join_type("cross").unwrap(), Cross);
        assert!(parse_join_type("unknown").is_err());
    }
}
