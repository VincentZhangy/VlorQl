//! Build a typed [`QueryPlan`] from canonical JSON.
//!
//! Today this is a thin `serde` path: once [`super::canonicalize`] has made
//! the Value schema-shaped, deserialization is the builder. Explicit
//! `build_expr` / `build_predicate` helpers replace this as they are added.

use std::fmt;
use vlorql_core::schema::{
    DataType, Expression, FromClause, JoinClause, JoinType, OrderByTerm, Predicate, Projection,
    QueryPlan,
};

// ------------------------------------------------------------------
// BuildError
// ------------------------------------------------------------------

/// A structured error from the JSON-to-`QueryPlan` builder.
///
/// Every error carries a `.path` string (e.g. `"where.left.type"`) so
/// callers can produce precise telemetry and prompt feedback.
#[derive(Debug, Clone)]
pub struct BuildError {
    /// Dot-separated path into the canonical JSON, e.g. `"where.left.type"`.
    pub path: String,
    /// Human-readable description of what went wrong.
    pub message: String,
}

impl BuildError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Append a field name to the path and produce a new error.
    fn at(self, field: &str) -> Self {
        let new_path = if self.path.is_empty() {
            field.to_owned()
        } else {
            format!("{}.{}", self.path, field)
        };
        Self {
            path: new_path,
            ..self
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at `{}`: {}", self.path, self.message)
    }
}

impl std::error::Error for BuildError {}

impl From<BuildError> for serde_json::Error {
    fn from(e: BuildError) -> Self {
        <serde_json::Error as serde::de::Error>::custom(e.to_string())
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Extract a string field from `obj`, returning a descriptive error if
/// the field is missing or not a string.
fn req_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, BuildError> {
    match obj.get(field) {
        None => Err(BuildError::new(path, format!("missing field `{field}`"))),
        Some(serde_json::Value::String(s)) => Ok(s),
        Some(other) => Err(BuildError::new(
            path,
            format!(
                "field `{field}` should be a string, got {}",
                type_name(other)
            ),
        )),
    }
}

/// Extract an optional string field from `obj`.
fn opt_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<&'a str> {
    obj.get(field).and_then(|v| v.as_str())
}

/// Extract an object from `obj` at `field`.
fn req_obj<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
    path: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, BuildError> {
    match obj.get(field) {
        None => Err(BuildError::new(path, format!("missing field `{field}`"))),
        Some(serde_json::Value::Object(m)) => Ok(m),
        Some(other) => Err(BuildError::new(
            path,
            format!(
                "field `{field}` should be an object, got {}",
                type_name(other)
            ),
        )),
    }
}

/// Extract an array from `obj` at `field`.
fn req_arr<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
    path: &str,
) -> Result<&'a [serde_json::Value], BuildError> {
    match obj.get(field) {
        None => Err(BuildError::new(path, format!("missing field `{field}`"))),
        Some(serde_json::Value::Array(a)) => Ok(a),
        Some(other) => Err(BuildError::new(
            path,
            format!(
                "field `{field}` should be an array, got {}",
                type_name(other)
            ),
        )),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// ------------------------------------------------------------------
// Entry point
// ------------------------------------------------------------------

/// Build a [`QueryPlan`] from a canonical [`serde_json::Value`].
///
/// # Errors
///
/// Returns [`BuildError`] on missing fields, type mismatches, or unknown
/// variants. The `.path` field pinpoints the location of the problem.
pub fn build_plan(value: &serde_json::Value) -> Result<QueryPlan, BuildError> {
    let obj = value
        .as_object()
        .ok_or_else(|| BuildError::new("", format!("expected object, got {}", type_name(value))))?;
    build_plan_from_obj(obj)
}

fn build_plan_from_obj(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<QueryPlan, BuildError> {
    let path = "";

    let select: Vec<Projection> = {
        let arr = req_arr(obj, "select", path)?;
        arr.iter()
            .enumerate()
            .map(|(i, v)| build_projection(v).map_err(|e| e.at(&format!("select[{i}]"))))
            .collect::<Result<Vec<_>, _>>()?
    };

    let from = build_from_clause(req_obj(obj, "from", path)?, "from")?;

    let r#where = obj
        .get("where")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at("where")))
        .transpose()?;

    let group_by = obj
        .get("group_by")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("group_by[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()?;

    let having = obj
        .get("having")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at("having")))
        .transpose()?;

    let order_by = obj
        .get("order_by")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_order_by_term(v).map_err(|e| e.at(&format!("order_by[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()?;

    let limit = obj.get("limit").and_then(|v| v.as_u64());
    let offset = obj.get("offset").and_then(|v| v.as_u64());

    let joins = obj
        .get("joins")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_join_clause(v).map_err(|e| e.at(&format!("joins[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let ctes = obj
        .get("ctes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_cte(v).map_err(|e| e.at(&format!("ctes[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    Ok(QueryPlan {
        select,
        from,
        r#where,
        group_by,
        having,
        order_by,
        limit,
        offset,
        joins,
        ctes,
    })
}

// ------------------------------------------------------------------
// Projection
// ------------------------------------------------------------------

fn build_projection(val: &serde_json::Value) -> Result<Projection, BuildError> {
    let obj = val.as_object().ok_or_else(|| {
        BuildError::new(
            "",
            format!("expected object for Projection, got {}", type_name(val)),
        )
    })?;
    let type_str = req_str(obj, "type", "")?;
    match type_str {
        "column_ref" => {
            let column = req_str(obj, "column", "")?.to_owned();
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            let alias = opt_str(obj, "alias").map(|s| s.to_owned());
            Ok(Projection::Column {
                table,
                column,
                alias,
            })
        }
        "expr" => {
            let e = req_obj(obj, "expression", "").or_else(|_| {
                // Accept bare `expr` field
                obj.get("expr")
                    .and_then(|v| v.as_object())
                    .ok_or_else(|| BuildError::new("", "missing `expression` field"))
            })?;
            let expression = build_expression(&serde_json::Value::Object(e.clone()))?;
            let alias = opt_str(obj, "alias").map(|s| s.to_owned());
            Ok(Projection::Expr { expression, alias })
        }
        "star" => {
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            Ok(Projection::Star { table })
        }
        other => Err(BuildError::new(
            "type",
            format!("unknown Projection variant `{other}`"),
        )),
    }
}

// ------------------------------------------------------------------
// Expression
// ------------------------------------------------------------------

/// Build an [`Expression`] from a canonical JSON value.
pub fn build_expression(val: &serde_json::Value) -> Result<Expression, BuildError> {
    let obj = match val.as_object() {
        Some(o) => o,
        None => {
            if val.is_null() {
                return Ok(Expression::Literal {
                    value: serde_json::Value::Null,
                    data_type: DataType::Null,
                });
            }
            // Try to infer from a bare value
            return build_literal_expr(val);
        }
    };

    let type_str = match obj.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => {
            // Infer type from present fields.
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
        "column_ref" | "ColumnRef" => {
            let column = req_str(obj, "column", "")?.to_owned();
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            Ok(Expression::ColumnRef { table, column })
        }
        "literal" | "Literal" => build_literal_from_obj(obj),
        "function_call" | "FunctionCall" => {
            let name = req_str(obj, "name", "")?.to_owned();
            let args_arr = req_arr(obj, "args", "")?;
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
        "binary_op" | "BinaryOp" => {
            let left = req_obj(obj, "left", "")
                .map(|o| build_expression(&serde_json::Value::Object(o.clone())))
                .or_else(|_| {
                    obj.get("left")
                        .map(|v| build_expression(v))
                        .ok_or_else(|| BuildError::new("left", "missing `left` field"))
                })??;
            let op_str = req_str(obj, "op", "")?;
            let op = parse_binary_op(op_str)?;
            let right = req_obj(obj, "right", "")
                .map(|o| build_expression(&serde_json::Value::Object(o.clone())))
                .or_else(|_| {
                    obj.get("right")
                        .map(|v| build_expression(v))
                        .ok_or_else(|| BuildError::new("right", "missing `right` field"))
                })??;
            Ok(Expression::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        }
        "star" | "Star" => Ok(Expression::Star),
        "subquery" | "SubQuery" => {
            let sub = req_obj(obj, "query", "")?;
            let query = build_plan_from_obj(sub)?;
            Ok(Expression::SubQuery {
                query: Box::new(query),
            })
        }
        other => Err(BuildError::new(
            "type",
            format!("unknown Expression variant `{other}`"),
        )),
    }
}

fn build_literal_expr(val: &serde_json::Value) -> Result<Expression, BuildError> {
    let data_type = match val {
        serde_json::Value::Null => DataType::Null,
        serde_json::Value::Bool(_) => DataType::Boolean,
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => DataType::Int,
        serde_json::Value::Number(_) => DataType::Float,
        serde_json::Value::String(_) => DataType::String,
        _ => {
            return Err(BuildError::new(
                "",
                format!("cannot infer Expression from bare value {:?}", val),
            ));
        }
    };
    Ok(Expression::Literal {
        value: val.clone(),
        data_type,
    })
}

fn build_literal_from_obj(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<Expression, BuildError> {
    let value = obj.get("value").cloned().unwrap_or(serde_json::Value::Null);
    let data_type_str = opt_str(obj, "data_type").unwrap_or("null");
    let data_type = parse_data_type(data_type_str)?;
    Ok(Expression::Literal { value, data_type })
}

// ------------------------------------------------------------------
// Predicate
// ------------------------------------------------------------------

/// Build a [`Predicate`] from a canonical JSON value.
pub fn build_predicate(val: &serde_json::Value) -> Result<Predicate, BuildError> {
    let obj = match val.as_object() {
        Some(o) => o,
        None => {
            return Err(BuildError::new(
                "",
                format!("expected object for Predicate, got {}", type_name(val)),
            ));
        }
    };

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
        "comparison" | "Comparison" => {
            let left =
                build_expression(obj.get("left").ok_or_else(|| {
                    BuildError::new("left", "missing `left` field on comparison")
                })?)
                .map_err(|e| e.at("left"))?;
            let op_str = req_str(obj, "op", "")?;
            let op = parse_comparison_op(op_str)?;
            let right =
                build_expression(obj.get("right").ok_or_else(|| {
                    BuildError::new("right", "missing `right` field on comparison")
                })?)
                .map_err(|e| e.at("right"))?;
            Ok(Predicate::Comparison { left, op, right })
        }
        "and" | "And" => {
            let left = build_predicate(
                obj.get("left")
                    .ok_or_else(|| BuildError::new("left", "missing `left` field on and"))?,
            )
            .map_err(|e| e.at("left"))?;
            let right = build_predicate(
                obj.get("right")
                    .ok_or_else(|| BuildError::new("right", "missing `right` field on and"))?,
            )
            .map_err(|e| e.at("right"))?;
            Ok(Predicate::And {
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        "or" | "Or" => {
            let left = build_predicate(
                obj.get("left")
                    .ok_or_else(|| BuildError::new("left", "missing `left` field on or"))?,
            )
            .map_err(|e| e.at("left"))?;
            let right = build_predicate(
                obj.get("right")
                    .ok_or_else(|| BuildError::new("right", "missing `right` field on or"))?,
            )
            .map_err(|e| e.at("right"))?;
            Ok(Predicate::Or {
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        "not" | "Not" => {
            let child = build_predicate(
                obj.get("child")
                    .ok_or_else(|| BuildError::new("child", "missing `child` field on not"))?,
            )
            .map_err(|e| e.at("child"))?;
            Ok(Predicate::Not {
                child: Box::new(child),
            })
        }
        "between" | "Between" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field on between"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let low = build_expression(
                obj.get("low")
                    .ok_or_else(|| BuildError::new("low", "missing `low` field on between"))?,
            )
            .map_err(|e| e.at("low"))?;
            let high = build_expression(
                obj.get("high")
                    .ok_or_else(|| BuildError::new("high", "missing `high` field on between"))?,
            )
            .map_err(|e| e.at("high"))?;
            Ok(Predicate::Between { expr, low, high })
        }
        "in" | "In" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field on in"))?,
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
                vlorql_core::schema::InTarget::Values(values)
            } else if let Some(sub_obj) = target_val.as_object() {
                let query = build_plan_from_obj(sub_obj)?;
                vlorql_core::schema::InTarget::SubQuery(Box::new(query))
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
        "like" | "Like" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field on like"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let pattern = req_str(obj, "pattern", "")?.to_owned();
            Ok(Predicate::Like { expr, pattern })
        }
        "is_null" | "IsNull" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field on is_null"))?,
            )
            .map_err(|e| e.at("expr"))?;
            Ok(Predicate::IsNull { expr })
        }
        "exists" | "Exists" => {
            let sub_obj = req_obj(obj, "query", "")?;
            let query = build_plan_from_obj(sub_obj)?;
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

// ------------------------------------------------------------------
// FromClause / JoinClause / OrderByTerm / CTE
// ------------------------------------------------------------------

fn build_from_clause(
    obj: &serde_json::Map<String, serde_json::Value>,
    path: &str,
) -> Result<FromClause, BuildError> {
    let table = req_str(obj, "table", path)?.to_owned();
    let alias = opt_str(obj, "alias").map(|s| s.to_owned());
    Ok(FromClause { table, alias })
}

fn build_join_clause(val: &serde_json::Value) -> Result<JoinClause, BuildError> {
    let obj = val.as_object().ok_or_else(|| {
        BuildError::new(
            "",
            format!("expected object for JoinClause, got {}", type_name(val)),
        )
    })?;
    let join_type_str = req_str(obj, "join_type", "")?;
    let join_type = parse_join_type(join_type_str)?;
    let right_table = build_from_clause(req_obj(obj, "right_table", "")?, "right_table")?;
    let on = build_predicate(
        obj.get("on")
            .ok_or_else(|| BuildError::new("on", "missing `on` field on join"))?,
    )
    .map_err(|e| e.at("on"))?;
    Ok(JoinClause {
        join_type,
        right_table,
        on,
    })
}

fn build_order_by_term(val: &serde_json::Value) -> Result<OrderByTerm, BuildError> {
    let obj = val.as_object().ok_or_else(|| {
        BuildError::new(
            "",
            format!("expected object for OrderByTerm, got {}", type_name(val)),
        )
    })?;
    let expr = build_expression(
        obj.get("expr")
            .ok_or_else(|| BuildError::new("expr", "missing `expr` field on order_by term"))?,
    )
    .map_err(|e| e.at("expr"))?;
    let descending = obj
        .get("descending")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(OrderByTerm { expr, descending })
}

#[allow(clippy::unnecessary_wraps)]
fn build_cte(
    val: &serde_json::Value,
) -> Result<vlorql_core::schema::CommonTableExpression, BuildError> {
    // CTE is rare: serde-deserialize to avoid maintaining parallel builder.
    serde_json::from_value(val.clone())
        .map_err(|e| BuildError::new("", format!("CTE deserialization: {e}")))
}

// ------------------------------------------------------------------
// Enum string parsers
// ------------------------------------------------------------------

fn parse_comparison_op(s: &str) -> Result<vlorql_core::schema::ComparisonOperator, BuildError> {
    use vlorql_core::schema::ComparisonOperator::*;
    match s {
        "eq" | "=" | "==" => Ok(Eq),
        "neq" | "!=" | "<>" => Ok(Neq),
        "gt" | ">" => Ok(Gt),
        "gte" | ">=" => Ok(Gte),
        "lt" | "<" => Ok(Lt),
        "lte" | "<=" => Ok(Lte),
        _ => Err(BuildError::new(
            "op",
            format!("unknown comparison operator `{s}`"),
        )),
    }
}

fn parse_binary_op(s: &str) -> Result<vlorql_core::schema::BinaryOperator, BuildError> {
    use vlorql_core::schema::BinaryOperator::*;
    match s {
        "add" | "+" => Ok(Add),
        "sub" | "-" => Ok(Sub),
        "mul" | "*" => Ok(Mul),
        "div" | "/" => Ok(Div),
        "mod" | "%" => Ok(Mod),
        _ => Err(BuildError::new(
            "op",
            format!("unknown binary operator `{s}`"),
        )),
    }
}

fn parse_join_type(s: &str) -> Result<JoinType, BuildError> {
    use vlorql_core::schema::JoinType::*;
    match s {
        "inner" | "INNER" => Ok(Inner),
        "left" | "LEFT" => Ok(Left),
        "right" | "RIGHT" => Ok(Right),
        "full" | "FULL" | "outer" | "OUTER" => Ok(Full),
        "cross" | "CROSS" => Ok(Cross),
        _ => Err(BuildError::new(
            "join_type",
            format!("unknown join type `{s}`"),
        )),
    }
}

fn parse_data_type(s: &str) -> Result<DataType, BuildError> {
    use vlorql_core::schema::DataType::*;
    match s {
        "int" | "integer" => Ok(Int),
        "float" | "double" | "decimal" => Ok(Float),
        "string" | "text" | "varchar" => Ok(String),
        "boolean" | "bool" => Ok(Boolean),
        "timestamp" | "datetime" => Ok(Timestamp),
        "null" | "NULL" => Ok(Null),
        _ => Err(BuildError::new(
            "data_type",
            format!("unknown data type `{s}`"),
        )),
    }
}

// ------------------------------------------------------------------
// Compatibility wrappers (return serde_json::Error for callers)
// ------------------------------------------------------------------

/// Deserialize a canonical JSON string into a [`QueryPlan`].
///
/// # Errors
///
/// Returns a `serde_json::Error` (wrapping [`BuildError`] when the builder
/// is used, or the underlying serde error for the fallback path).
pub fn from_canonical_str(canonical: &str) -> Result<QueryPlan, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(canonical)?;
    from_canonical_value(&value)
}

/// Deserialize a canonical [`serde_json::Value`] into a [`QueryPlan`].
///
/// Uses the explicit builder when possible; falls back to serde for types
/// not yet covered by the builder (e.g. CTEs).
///
/// # Errors
///
/// Returns a `serde_json::Error` (wrapping [`BuildError`]) on failure.
pub fn from_canonical_value(canonical: &serde_json::Value) -> Result<QueryPlan, serde_json::Error> {
    build_plan(canonical).map_err(Into::into)
}
