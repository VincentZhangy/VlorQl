//! Expression normalization.
//!
//! Normalizes expression structures and predicate shapes:
//!
//! - Injects missing `type` discriminator for Expression-like objects
//!   (ColumnRef, Literal, FunctionCall)
//! - Injects missing `type` discriminator for Predicate-like objects
//!   (Comparison)
//! - Fixes array-valued `left`/`right`/`child` in predicates
//! - Injects missing `right` field on comparison predicates
//! - Simplifies single-child `and`/`or` predicates
//!
//! ## Sub-modules
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `literal` | Literal expression normalization (canonical type, aggregate shorthand) |
//! | `predicate` | Predicate normalization (type injection, array unwrap, missing right, simplify) |
//! | `case` | Malformed `function_call("case")` → proper `case` expression |

mod case;
mod literal;
mod predicate;

// Re-export so `normalize_impl` below can reference these functions
// directly.  The test module (`#[cfg(test)] mod tests`) picks them up
// via `use super::*;`.
use case::normalize_malformed_case_expression_in_map;
#[cfg(test)]
use literal::repair_expression_value;
use predicate::normalize_predicate;
#[cfg(test)]
use predicate::{
    inject_missing_right, repair_predicate_type, simplify_single_child, unwrap_array_sides,
};

use serde_json::Value;

/// Known expression type names (not predicate-like — excluded from predicate detection).
const EXPR_TYPES: &[&str] = &[
    "literal",
    "column_ref",
    "function_call",
    "binary_op",
    "star",
    "subquery",
    "case",
    "window_function",
    "expr",
];

/// Arithmetic operator aliases: `type` tag → canonical `op` value.
const ARITH_OPS: &[(&str, &str)] = &[
    ("multiply", "mul"),
    ("add", "add"),
    ("subtract", "sub"),
    ("minus", "sub"),
    ("divide", "div"),
];

/// Expression type keys used for key-as-type detection.
const EXPR_TYPE_KEYS: &[&str] = &[
    "function_call",
    "column_ref",
    "binary_op",
    "literal",
    "star",
    "subquery",
    "case",
    "window_function",
];

/// Full expression normalization for a value tree.
///
/// Recursively normalizes all predicates and expressions.
#[must_use]
pub fn normalize(val: &mut Value) -> bool {
    normalize_impl(val)
}

fn normalize_impl(val: &mut Value) -> bool {
    let mut changed = false;

    match val {
        Value::Object(map) => {
            // Handle malformed `case` expression in predicate position BEFORE
            // the `is_predicate_like` check: wrap it in a comparison so the
            // builder doesn't fail with "unknown Predicate variant `case`".
            // This must NOT recurse into the wrapped case's children (which
            // would re-detect it and cause infinite recursion / stack overflow).
            let pred_type = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if pred_type == "case" && !map.contains_key("when_thens") {
                let comparison = serde_json::json!({
                    "type": "comparison",
                    "left": Value::Object(std::mem::take(map)),
                    "op": "ne",
                    "right": {"type": "literal", "value": null, "data_type": "null"}
                });
                if let Value::Object(new_map) = comparison {
                    *map = new_map;
                }
                changed = true;
                // No child recursion — the builder handles Case expressions
                // inside comparison predicates.  Skipping recursion prevents
                // the infinite loop that would otherwise occur because the
                // wrapped `case` in `left` would be re-detected as predicate-like.
                return changed;
            }

            let is_predicate_like = (!pred_type.is_empty() && !EXPR_TYPES.contains(&pred_type))
                || (map.contains_key("left") && map.contains_key("op"))
                || (pred_type.is_empty()
                    && (map.contains_key("not")
                        || map.contains_key("exists")
                        || map.contains_key("and")
                        || map.contains_key("or")));

            if is_predicate_like {
                // Preserve non-predicate fields that may be dropped by
                // normalize_predicate's clear+rebuild operations.
                let preserved: Vec<(String, Value)> = map
                    .iter()
                    .filter(|(k, _)| {
                        !matches!(
                            k.as_str(),
                            "type"
                                | "left"
                                | "right"
                                | "op"
                                | "child"
                                | "expr"
                                | "low"
                                | "high"
                                | "target"
                                | "pattern"
                                | "query"
                        )
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                let mut tmp = Value::Object(std::mem::take(map));
                changed |= normalize_predicate(&mut tmp);
                if let Value::Object(mut m) = tmp {
                    for (k, v) in preserved {
                        m.entry(k).or_insert(v);
                    }
                    *map = m;
                }
            }

            // Fix: LLM puts array of column names instead of a single string
            // (e.g. `"column":["name","email"]` in a column_ref). Use first element.
            if let Some(col) = map.get("column").and_then(|v| v.as_array())
                && let Some(first) = col.first().and_then(|v| v.as_str())
            {
                map.insert("column".to_owned(), Value::String(first.to_owned()));
                changed = true;
            }

            // Fix: LLM sometimes uses dot-qualified column names like
            // `"column":"users.id"` instead of separate `table` and `column`
            // fields.  Split on the first `.` when no `table` field exists.
            if !map.contains_key("table")
                && let Some(col) = map.get("column").and_then(|v| v.as_str())
                && let Some(dot) = col.find('.')
                && dot > 0
                && dot + 1 < col.len()
            {
                let table = col[..dot].to_owned();
                let column = col[dot + 1..].to_owned();
                map.insert("table".to_owned(), Value::String(table));
                map.insert("column".to_owned(), Value::String(column));
                changed = true;
            }

            // Fix: LLMs sometimes emit aggregate function names as `type`
            let type_str = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if !type_str.is_empty()
                && !matches!(
                    type_str.as_str(),
                    "function_call"
                        | "column_ref"
                        | "literal"
                        | "binary_op"
                        | "star"
                        | "subquery"
                        | "comparison"
                        | "and"
                        | "or"
                        | "not"
                        | "between"
                        | "in"
                        | "like"
                        | "is_null"
                        | "exists"
                )
                && vlorql_core::function::is_known_function(&type_str)
                && let Some(args) = map.remove("args")
            {
                map.insert("type".to_owned(), Value::String("function_call".to_owned()));
                map.insert("name".to_owned(), Value::String(type_str));
                map.insert("args".to_owned(), args);
                changed = true;
            }

            // Fix: `string_agg` with only 1 arg — inject default `','` delimiter.
            if map.get("name").and_then(|n| n.as_str()) == Some("string_agg")
                && map.get("type").and_then(|t| t.as_str()) == Some("function_call")
                && let Some(args) = map.get_mut("args").and_then(|a| a.as_array_mut())
                && args.len() == 1
            {
                args.push(
                    serde_json::json!({"type": "literal", "value": ",", "data_type": "string"}),
                );
                changed = true;
            }

            // Fix: `row_number()` with args — the function takes zero arguments.
            if map.get("name").and_then(|n| n.as_str()) == Some("row_number")
                && map.get("type").and_then(|t| t.as_str()) == Some("function_call")
                && map.contains_key("args")
            {
                map.remove("args");
                changed = true;
            }

            // Fix: LLM sometimes embeds SQL syntax inside a function name
            // (e.g. `"name": "EXTRACT(MONTH FROM)"` instead of the canonical
            // `"name": "extract"` with `"month"` as a separate literal arg).
            // Detect function names that contain `(`, split on `(`, and if
            // the trailing part matches `KEYWORD FROM` syntax, inject the
            // keyword as an additional argument.
            if map.get("type").and_then(|t| t.as_str()) == Some("function_call")
                && let Some(name) = map.get("name").and_then(|n| n.as_str())
                && let Some(paren_pos) = name.find('(')
            {
                // Extract base name and keyword BEFORE mutating map.
                let base = name[..paren_pos].trim().to_ascii_lowercase();
                let keyword: Option<String> = {
                    let rest = &name[paren_pos + 1..];
                    rest.to_ascii_lowercase()
                        .find("from")
                        .map(|from_pos| rest[..from_pos].trim().to_ascii_lowercase())
                        .filter(|k| !k.is_empty())
                };
                map.insert("name".to_owned(), Value::String(base));
                if let Some(kw) = keyword
                    && let Some(args) = map.get_mut("args").and_then(|a| a.as_array_mut())
                {
                    args.insert(
                        0,
                        serde_json::json!({
                            "type": "literal",
                            "value": kw,
                            "data_type": "string"
                        }),
                    );
                }
                changed = true;
            }

            // Fix: LLMs sometimes emit arithmetic operator names as `type`.
            if let Some(type_str) = map
                .get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_lowercase())
                && let Some(&(_, op)) = ARITH_OPS.iter().find(|&&(name, _)| name == type_str)
                && let Some(args) = map.remove("args").and_then(|v| v.as_array().cloned())
            {
                map.insert("type".to_owned(), Value::String("binary_op".to_owned()));
                map.insert("op".to_owned(), Value::String(op.to_owned()));
                if args.len() >= 2 {
                    map.insert("left".to_owned(), args[0].clone());
                    map.insert("right".to_owned(), args[1].clone());
                } else if args.len() == 1 {
                    map.insert("left".to_owned(), args[0].clone());
                    map.insert("right".to_owned(), args[0].clone());
                }
                changed = true;
            }

            // Handle case where expression type is used as a KEY instead of
            // the value of `type`.
            if !map.contains_key("type") {
                for &expr_key in EXPR_TYPE_KEYS {
                    if let Some(Value::Object(inner)) = map.remove(expr_key) {
                        map.insert("type".to_owned(), Value::String(expr_key.to_owned()));
                        for (k, v) in inner {
                            map.entry(k).or_insert(v);
                        }
                        changed = true;
                        break;
                    }
                }
            }

            // Convert malformed CASE expression from the LLM.
            let needs_case_normalization = {
                let name = map.get("name").and_then(|n| n.as_str());
                let type_ = map.get("type").and_then(|t| t.as_str());
                name == Some("case") && type_ == Some("function_call")
            };
            if needs_case_normalization {
                changed |= normalize_malformed_case_expression_in_map(map);
            }

            // Fix: LLM sometimes puts bare predicates in `when_thens`
            // instead of canonical `{"when":..., "then":...}` pairs.
            // E.g. `{"when_thens": [{"left":...,"op":">","right":...}]}`
            // should be `{"when_thens": [{"when":{...},"then":{...}}]}`.
            if let Some(when_thens) = map.get_mut("when_thens").and_then(|v| v.as_array_mut()) {
                for item in when_thens.iter_mut() {
                    let Some(obj) = item.as_object_mut() else {
                        continue;
                    };
                    if obj.contains_key("when") || obj.contains_key("then") {
                        continue; // already canonical
                    }
                    // If this looks like a predicate (has left+op+right),
                    // use it as the `when` condition with a boolean `then`.
                    if obj.contains_key("left") && obj.contains_key("op") {
                        let existing = std::mem::take(obj);
                        obj.insert("when".to_owned(), Value::Object(existing));
                        obj.insert("then".to_owned(),
                            serde_json::json!({"type": "literal", "value": true, "data_type": "boolean"}));
                        changed = true;
                    }
                }
            }

            // Recurse into children.
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in &keys {
                if let Some(v) = map.get_mut(key) {
                    changed |= normalize_impl(v);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                changed |= normalize_impl(v);
            }
        }
        _ => {}
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── repair_expression_value ───────────────────────────────────

    #[test]
    fn injects_column_ref_type() {
        let mut val = json!({"column": "name", "table": "users"});
        assert!(repair_expression_value(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("column_ref"));
    }

    #[test]
    fn injects_literal_type() {
        let mut val = json!({"value": 42, "data_type": "int"});
        assert!(repair_expression_value(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("literal"));
    }

    #[test]
    fn injects_function_call_type() {
        let mut val = json!({"name": "count", "args": [{"type": "star"}]});
        assert!(repair_expression_value(&mut val));
        assert_eq!(
            val.get("type").and_then(|v| v.as_str()),
            Some("function_call")
        );
    }

    #[test]
    fn expression_already_has_type() {
        let mut val = json!({"type": "column_ref", "column": "name"});
        assert!(!repair_expression_value(&mut val));
    }

    #[test]
    fn expression_no_recognizable_fields() {
        let mut val = json!({"unknown": "field"});
        assert!(!repair_expression_value(&mut val));
    }

    #[test]
    fn integer_literal_normalizes_to_int_consistently() {
        let mut v = serde_json::json!({"type": "integer", "value": 5});
        assert!(repair_expression_value(&mut v));
        assert_eq!(v.get("data_type").and_then(|d| d.as_str()), Some("int"),);
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("literal"));
    }

    #[test]
    fn number_literal_disambiguates_by_value() {
        let mut i = serde_json::json!({"type": "number", "value": 3});
        assert!(repair_expression_value(&mut i));
        assert_eq!(i.get("data_type").and_then(|d| d.as_str()), Some("int"));

        let mut f = serde_json::json!({"type": "number", "value": 3.5});
        assert!(repair_expression_value(&mut f));
        assert_eq!(f.get("data_type").and_then(|d| d.as_str()), Some("float"));
    }

    // ── repair_predicate_type ─────────────────────────────────────

    #[test]
    fn injects_comparison_type() {
        let mut val = json!({"left": {"column": "age"}, "op": "gt", "right": {"value": 18}});
        assert!(repair_predicate_type(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
    }

    #[test]
    fn predicate_already_has_type() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt"});
        assert!(!repair_predicate_type(&mut val));
    }

    // ── unwrap_array_sides ────────────────────────────────────────

    #[test]
    fn unwraps_and_left_array() {
        let mut val = json!({"type": "and", "left": [{"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}], "right": {"type": "comparison", "left": {"column": "b"}, "op": "gt", "right": {"value": 2}}});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("left").unwrap().is_object());
    }

    #[test]
    fn unwraps_not_child_array() {
        let mut val = json!({"type": "not", "child": [{"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}]});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("child").unwrap().is_object());
    }

    #[test]
    fn unwraps_comparison_left_array() {
        let mut val = json!({"type": "comparison", "left": [{"column": "age"}], "op": "gt", "right": {"value": 18}});
        assert!(unwrap_array_sides(&mut val));
        assert!(val.get("left").unwrap().is_object());
    }

    // ── inject_missing_right ──────────────────────────────────────

    #[test]
    fn injects_missing_right_on_comparison() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt"});
        assert!(inject_missing_right(&mut val));
        assert!(val.get("right").is_some());
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("literal")
        );
    }

    #[test]
    fn does_not_inject_when_right_exists() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}});
        assert!(!inject_missing_right(&mut val));
    }

    // ── simplify_single_child ─────────────────────────────────────

    #[test]
    fn simplifies_and_without_right() {
        let mut val = json!({"type": "and", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}});
        assert!(simplify_single_child(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
    }

    #[test]
    fn does_not_simplify_and_with_both_sides() {
        let mut val =
            json!({"type": "and", "left": {"type": "comparison"}, "right": {"type": "comparison"}});
        assert!(!simplify_single_child(&mut val));
    }

    // ── normalize_predicate ───────────────────────────────────────

    #[test]
    fn full_predicate_normalize() {
        let mut val = json!({
            "left": {"column": "a"},
            "op": "=",
            "right": [{"value": 1}]
        });
        assert!(normalize_predicate(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("comparison"));
        assert!(val.get("right").unwrap().is_object());
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("literal")
        );
        assert_eq!(
            val.pointer("/left/type").and_then(|v| v.as_str()),
            Some("column_ref")
        );
    }

    #[test]
    fn recursive_predicate_normalize() {
        let mut val = json!({
            "type": "and",
            "left": [{"left": {"column": "a"}, "op": "=", "right": {"value": 1}}],
            "right": [{"left": {"column": "b"}, "op": ">", "right": {"value": 2}}]
        });
        assert!(normalize_predicate(&mut val));
        assert!(val.get("left").unwrap().is_object());
        assert!(val.get("right").unwrap().is_object());
        assert_eq!(
            val.pointer("/left/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
        assert_eq!(
            val.pointer("/right/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    // ── normalize (top-level) ─────────────────────────────────────

    #[test]
    fn full_normalize_tree() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "and",
                "left": [{"left": {"column": "age"}, "op": ">", "right": {"value": 18, "data_type": "integer"}}],
                "right": [{"left": {"column": "status"}, "op": "=", "right": {"value": "active"}}]
            }
        });
        assert!(normalize(&mut val));
        let where_obj = val.get("where").unwrap().as_object().unwrap();
        assert!(where_obj.get("left").unwrap().is_object());
        assert!(where_obj.get("right").unwrap().is_object());
        assert_eq!(
            val.pointer("/where/left/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
        assert_eq!(
            val.pointer("/where/right/type").and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "comparison",
                "left": {"type": "column_ref", "column": "age"},
                "op": "gt",
                "right": {"type": "literal", "value": 18, "data_type": "int"}
            }
        });
        assert!(!normalize(&mut val));
    }

    #[test]
    fn normalizes_function_call_case_expression() {
        let mut val = json!({
            "select": [
                {"type": "column_ref", "table": "orders", "column": "id"},
                {"type": "expr", "expression": {
                    "function_call": {
                        "name": "case",
                        "args": [
                            {"type": "comparison", "left": {"type": "column_ref", "column": "total", "table": "orders"}, "op": "=", "right": {"type": "literal", "value": 1500, "data_type": "float"}},
                            {"type": "literal", "value": "high", "data_type": "string"},
                            null
                        ]
                    },
                    "alias": "category"
                }, "alias": null}
            ],
            "from": {"table": "orders"}
        });
        assert!(normalize(&mut val));
        let select = val.get("select").unwrap().as_array().unwrap();
        let item2 = select[1].as_object().unwrap();
        assert_eq!(item2.get("type").and_then(|t| t.as_str()), Some("expr"));
        let expr = item2.get("expression").unwrap().as_object().unwrap();
        assert_eq!(expr.get("type").and_then(|t| t.as_str()), Some("case"));
        assert!(expr.contains_key("when_thens"));
    }

    #[test]
    fn normalizes_like_op_from_comparison_to_like_predicate() {
        // Regression: LLM outputs {"type":"comparison","op":"like","right":{"value":"%example.com","data_type":"string"}}
        // instead of the canonical {"type":"like","expr":...,"pattern":"%example.com"}.
        // The normalize pipeline must convert it so the builder doesn't fail
        // with "unknown comparison operator `like`".
        let mut val = serde_json::json!({
            "type": "comparison",
            "left": {"type": "column_ref", "column": "email", "table": "users"},
            "op": "like",
            "right": {"type": "literal", "value": "%example.com", "data_type": "string"}
        });
        assert!(normalize_predicate(&mut val));
        assert_eq!(val.get("type").and_then(|t| t.as_str()), Some("like"));
        let pattern = val.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        assert_eq!(pattern, "%example.com");
        assert_eq!(
            val.pointer("/expr/column").and_then(|c| c.as_str()),
            Some("email")
        );
    }

    #[test]
    fn normalizes_ilike_op_to_like_predicate() {
        let mut val = serde_json::json!({
            "type": "comparison",
            "left": {"type": "column_ref", "column": "name"},
            "op": "ilike",
            "right": {"type": "literal", "value": "%test%", "data_type": "string"}
        });
        assert!(normalize_predicate(&mut val));
        assert_eq!(val.get("type").and_then(|t| t.as_str()), Some("like"));
        assert_eq!(val.get("pattern").and_then(|p| p.as_str()), Some("%test%"));
    }
}
