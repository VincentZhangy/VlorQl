use super::BuildError;
use super::helpers::{
    opt_str, parse_binary_op, parse_comparison_op, parse_data_type, req_arr, req_obj, req_str,
    type_name,
};
use serde_json::Value;
use vlorql_core::schema::{DataType, Expression, InTarget, Predicate};

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
    let dt_str = obj
        .get("data_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
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

/// Build an [`Expression`](vlorql_core::schema::Expression) from a canonical JSON value.
///
/// Accepts both objects (with `type` discriminator) and bare values
/// (numbers, strings, booleans, nulls) which are inferred as literals.
///
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::build_expression;
/// use serde_json::json;
///
/// let expr = build_expression(&json!({"type": "column_ref", "column": "id"})).unwrap();
/// ```
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
                args: Box::new(args),
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
            let query = crate::parser_v2::builder::query_builder::build_plan_from_obj(sub)?;
            Ok(Expression::SubQuery {
                query: Box::new(query),
            })
        }
        "case" => {
            // CASE WHEN expression
            let operand = match obj.get("operand") {
                Some(val) if !val.is_null() => Some(Box::new(
                    build_expression(val).map_err(|e| e.at("operand"))?,
                )),
                _ => None,
            };
            let when_thens_arr = req_arr(
                obj.get("when_thens")
                    .ok_or_else(|| BuildError::new("", "missing `when_thens` field on case"))?,
                "when_thens",
            )?;
            let mut when_thens = Vec::new();
            for (i, item) in when_thens_arr.iter().enumerate() {
                let item_obj = req_obj(item, &format!("when_thens[{i}]"))?;
                let when = build_expression(item_obj.get("when").ok_or_else(|| {
                    BuildError::new(format!("when_thens[{i}]"), "missing `when` field")
                })?)
                .map_err(|e| e.at(&format!("when_thens[{i}].when")))?;
                let then = build_expression(item_obj.get("then").ok_or_else(|| {
                    BuildError::new(format!("when_thens[{i}]"), "missing `then` field")
                })?)
                .map_err(|e| e.at(&format!("when_thens[{i}].then")))?;
                when_thens.push(vlorql_core::schema::WhenThen { when, then });
            }
            let else_result = match obj.get("else_result") {
                Some(val) if !val.is_null() => Some(Box::new(
                    build_expression(val).map_err(|e| e.at("else_result"))?,
                )),
                _ => None,
            };
            Ok(Expression::Case {
                operand,
                when_thens: Box::new(when_thens),
                else_result,
            })
        }
        // `comparison` is a Predicate type, not an Expression.  LLMs often
        // emit it inside CASE WHEN clauses.  Convert to BinaryOp here so
        // the expression builder still succeeds even if normalization did
        // not catch it (e.g. when plans come from serde directly).
        "comparison" | "Comparison" => {
            let left_val = obj
                .get("left")
                .ok_or_else(|| BuildError::new("", "missing `left` field on comparison"))?;
            let left = build_expression(left_val).map_err(|e| e.at("left"))?;
            let op_str = req_str(obj, "op", "")?;
            // Map comparison operators to binary operators.
            // Both share `eq`, `neq`, `gt`, `gte`, `lt`, `lte`.
            let op = parse_binary_op(op_str)?;
            let right_val = obj
                .get("right")
                .ok_or_else(|| BuildError::new("", "missing `right` field on comparison"))?;
            let right = build_expression(right_val).map_err(|e| e.at("right"))?;
            Ok(Expression::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
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
                    args: Box::new(args),
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

/// Build a [`Predicate`](vlorql_core::schema::Predicate) from a canonical JSON value.
///
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::expr_builder::build_predicate;
/// use serde_json::json;
///
/// let pred = build_predicate(&json!({"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"column": "id"}})).unwrap();
/// ```
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
            Ok(Predicate::Comparison {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
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
            Ok(Predicate::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
            })
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
                let query = crate::parser_v2::builder::query_builder::build_plan_from_obj(sub_obj)?;
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
            Ok(Predicate::In {
                expr: Box::new(expr),
                target,
            })
        }
        "like" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            let pattern = req_str(obj, "pattern", "")?.to_owned();
            Ok(Predicate::Like {
                expr: Box::new(expr),
                pattern,
            })
        }
        "is_null" => {
            let expr = build_expression(
                obj.get("expr")
                    .ok_or_else(|| BuildError::new("expr", "missing `expr` field"))?,
            )
            .map_err(|e| e.at("expr"))?;
            Ok(Predicate::IsNull {
                expr: Box::new(expr),
            })
        }
        "true" => Ok(Predicate::True),
        "false" => Ok(Predicate::False),
        "exists" => {
            let sub = req_obj(
                obj.get("query")
                    .ok_or_else(|| BuildError::new("query", "missing `query` field on exists"))?,
                "query",
            )?;
            let query = crate::parser_v2::builder::query_builder::build_plan_from_obj(sub)?;
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
