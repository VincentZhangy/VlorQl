use serde_json::Value;
use super::BuildError;
use vlorql_core::schema::{BinaryOperator, ComparisonOperator, DataType, JoinType};

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
        "like" => Ok(Like),
        "ilike" => Ok(ILike),
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
        "and" => Ok(And),
        "or" => Ok(Or),
        "eq" => Ok(Eq),
        "neq" => Ok(Neq),
        "gt" => Ok(Gt),
        "lt" => Ok(Lt),
        "gte" => Ok(Gte),
        "lte" => Ok(Lte),
        "like" => Ok(Like),
        "ilike" => Ok(ILike),
        _ => Err(BuildError::new(
            "op",
            format!("unknown binary operator `{s}`"),
        )),
    }
}

/// Parse a join type string.
pub fn parse_join_type(s: &str) -> Result<JoinType, BuildError> {
    use JoinType::*;
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
///
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::parse_data_type;
/// use vlorql_core::schema::DataType;
///
/// assert_eq!(parse_data_type("int").unwrap(), DataType::Int);
/// assert_eq!(parse_data_type("decimal").unwrap(), DataType::Decimal);
/// ```
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
        "decimal" => Ok(Decimal),
        "array" => Ok(Array),
        "jsonb" => Ok(Jsonb),
        "blob" => Ok(Blob),
        "date" => Ok(Date),
        other => Err(BuildError::new(
            "data_type",
            format!("unknown data type `{other}`"),
        )),
    }
}
