//! Literal expression normalization.
//!
//! Handles canonical data-type inference and fix-up for literal-like
//! JSON objects.  All items are `pub(super)` — visible within the
//! `expr` module but not outside it.

use serde_json::Value;

use crate::parser_v2::normalize::common::canonical_data_type;

/// Maps a raw literal type tag plus its JSON value to the canonical
/// `data_type` string. The ambiguous `"number"` tag is disambiguated by
/// inspecting whether the value is integral, so both normalization paths
/// agree on `int` vs `float`.
#[must_use]
pub(super) fn canonical_literal_type(type_val: &str, value: Option<&Value>) -> &'static str {
    canonical_data_type(type_val, value).unwrap_or("null")
}

/// Convert LLM type aliases (string, integer, number, float, boolean, null)
/// to the canonical literal format:
///   {"type": "string", "value": "..."} → {"type": "literal", "value": "...", "data_type": "string"}
///   {"type": "integer", "value": 42}   → {"type": "literal", "value": 42, "data_type": "int"}
#[must_use]
pub(super) fn fix_literal_type_aliases(val: &mut Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let type_val = match obj.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !matches!(
        type_val,
        "string" | "integer" | "number" | "float" | "boolean" | "null"
    ) {
        return false;
    }
    if !obj.contains_key("value") {
        return false;
    }
    let canonical_dt = canonical_literal_type(type_val, obj.get("value"));
    obj.insert("type".to_owned(), Value::String("literal".to_owned()));
    obj.insert(
        "data_type".to_owned(),
        Value::String(canonical_dt.to_owned()),
    );
    true
}

/// Adds missing `"type"` tags to Expression-like JSON objects.
///
/// The LLM frequently omits the `type` discriminator from `ColumnRef`,
/// `Literal`, and `FunctionCall` objects.  This function infers the
/// correct tag from the present fields so that serde can deserialize
/// the value as an [`Expression`](vlorql_core::schema::Expression)(vlorql_core::schema::Expression).
///
/// Returns `true` if any change was made.
#[must_use]
pub(super) fn repair_expression_value(val: &mut Value) -> bool {
    // Fix: `{"type": "expr", "expression": {...}}` is a Projection::Expr format,
    // not a valid Expression. The LLM sometimes uses this format in expression
    // contexts (like inside WHERE predicates). Unwrap the inner expression.
    if let Some(obj) = val.as_object()
        && obj.get("type").and_then(|t| t.as_str()) == Some("expr")
        && obj.contains_key("expression")
        && let Some(inner) = obj.get("expression").cloned()
    {
        *val = inner;
        return true;
    }

    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    // Fix: `{"type":"column_ref",...,"expr":{...}}` — LLM sometimes sets
    // type=column_ref but includes an expr field (e.g. a function_call).
    // The expr field conflicts with column_ref; fix by changing to expr type.
    if obj.get("type").and_then(|t| t.as_str()) == Some("column_ref")
        && obj.contains_key("expr")
        && let Some(expr_val) = obj.get("expr")
        && expr_val.is_object()
    {
        let mut new_obj = serde_json::Map::new();
        new_obj.insert("type".to_owned(), Value::String("expr".to_owned()));
        new_obj.insert("expression".to_owned(), expr_val.clone());
        if let Some(alias) = obj.get("alias").cloned() {
            new_obj.insert("alias".to_owned(), alias);
        }
        *val = Value::Object(new_obj);
        return true;
    }

    // Fix: LLM sometimes uses {"type": "string", "value": "..."} or {"type": "integer", "value": N}
    // instead of the canonical {"type": "literal", "value": "...", "data_type": "string"}.
    if let Some(type_val) = obj
        .get("type")
        .and_then(|t| t.as_str())
        .map(|s| s.to_owned())
        && (type_val == "string"
            || type_val == "integer"
            || type_val == "number"
            || type_val == "float"
            || type_val == "boolean"
            || type_val == "null")
    {
        let value = obj.get("value").cloned().unwrap_or(Value::Null);
        let canonical_dt = canonical_literal_type(type_val.as_str(), Some(&value));
        obj.insert("type".to_owned(), Value::String("literal".to_owned()));
        obj.insert("value".to_owned(), value);
        obj.insert(
            "data_type".to_owned(),
            Value::String(canonical_dt.to_owned()),
        );
        return true;
    }

    // Fix: LLM sometimes uses aggregate shorthand like `{"type": "count", "expr": "*"}`
    // instead of canonical `{"type": "function_call", "name": "count", "args": [{"type": "star"}]}`.
    // Common aggregate names: count, sum, avg, min, max, array_agg, string_agg.
    if let Some(type_val) = obj
        .get("type")
        .and_then(|t| t.as_str())
        .map(|s| s.to_owned())
        && (type_val == "count"
            || type_val == "sum"
            || type_val == "avg"
            || type_val == "min"
            || type_val == "max"
            || type_val == "array_agg"
            || type_val == "string_agg"
            || type_val == "json_agg"
            || type_val == "jsonb_agg")
    {
        let name = type_val.clone();
        obj.insert("type".to_owned(), Value::String("function_call".to_owned()));
        obj.insert("name".to_owned(), Value::String(name));
        // Ensure args array exists (may be derived from `expr` or
        // `function_call` field  — LLM sometimes nests them).
        if !obj.contains_key("args") {
            // Try `function_call.args` first (nested format):
            // {"type":"string_agg","function_call":{"name":"concat","args":["..."]}}
            let args = obj
                .remove("function_call")
                .and_then(|fc| {
                    let fc_obj = fc.as_object()?;
                    let name = fc_obj.get("name").and_then(|n| n.as_str())?;
                    // Use the nested function_call's name if the shortcut name is generic.
                    let current_name = obj.get("name").and_then(|n| n.as_str());
                    if current_name == Some("string_agg") && name != "string_agg" {
                        // e.g. string_agg(function_call(name:"concat",...)) → keep name=concat
                        // Actually, string_agg is the aggregate wrapper; keep it.
                        // But use the inner function_call's name for the actual aggregate.
                    }
                    let args_arr = fc_obj.get("args")?;
                    Some(Value::Array(
                        if let Some(arr) = args_arr.as_array() {
                            arr.clone()
                        } else {
                            vec![args_arr.clone()]
                        },
                    ))
                })
                .or_else(|| {
                    let expr_val = obj.remove("expr").unwrap_or(Value::Null);
                    if expr_val.is_string() && expr_val.as_str() == Some("*") {
                        Some(serde_json::json!([{"type": "star"}]))
                    } else if !expr_val.is_null() {
                        Some(serde_json::json!([expr_val]))
                    } else {
                        None
                    }
                })
                .unwrap_or(Value::Array(Vec::new()));
            obj.insert("args".to_owned(), args);
        }
        return true;
    }

    if obj.contains_key("type") {
        return false;
    }

    // ColumnRef: has `column` (and optionally `table`)
    if obj.contains_key("column") {
        // Fix: LLM sometimes puts an array of column names instead of a
        // single string (e.g. `"column":["name","email"]`). Use the first
        // element as the column name.
        if let Some(arr) = obj.get("column").and_then(|v| v.as_array())
            && let Some(first) = arr.first().and_then(|v| v.as_str())
        {
            obj.insert("column".to_owned(), Value::String(first.to_owned()));
        }
        // Fix: LLM hallucinates aggregate-derived column names like
        // `"order_count"`, `"total_sales"`, `"product_sum"` — these are
        // computed aggregates, not real columns.  Convert to function_call
        // so the validator can process them correctly.
        if let Some(col_name) = obj.get("column").and_then(|v| v.as_str())
        {
            let lower = col_name.to_ascii_lowercase();
            // Common aggregate-derived suffix patterns.
            let agg_fn = if lower.ends_with("_count") {
                Some(("count", "*"))
            } else if lower.ends_with("_total") || lower.ends_with("_sum") {
                Some(("sum", "id"))
            } else if lower.ends_with("_avg") {
                Some(("avg", "id"))
            } else if lower.ends_with("_min") {
                Some(("min", "id"))
            } else if lower.ends_with("_max") {
                Some(("max", "id"))
            } else {
                None
            };
            if let Some((fn_name, arg)) = agg_fn {
                obj.insert("type".to_owned(), Value::String("function_call".to_owned()));
                obj.insert("name".to_owned(), Value::String(fn_name.to_owned()));
                let args = if arg == "*" {
                    vec![serde_json::json!({"type": "star"})]
                } else {
                    vec![serde_json::json!({"type": "column_ref", "column": arg})]
                };
                obj.insert("args".to_owned(), Value::Array(args));
                return true;
            }
        }
        obj.insert("type".to_owned(), Value::String("column_ref".to_owned()));
        return true;
    }

    // Literal: has `value`
    if obj.contains_key("value") {
        obj.insert("type".to_owned(), Value::String("literal".to_owned()));
        return true;
    }

    // FunctionCall: has `name` and `args`
    if obj.contains_key("name") && obj.contains_key("args") {
        obj.insert("type".to_owned(), Value::String("function_call".to_owned()));
        return true;
    }

    false
}
