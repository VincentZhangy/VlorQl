//! Normalize pipeline: orchestrates all normalization stages.
//!
//! The pipeline applies a sequence of normalizers to transform a
//! messy LLM JSON value into a canonical form that the builder layer
//! can consume.
//!
//! Current stages:
//!
//! 1. **Field aliases** — rename non-standard field names to canonical
//!    QueryPlan names (e.g. `filter` → `where`, `projection` → `select`).
//! 2. **Structure** — normalize array/object shapes, WHERE/select/from/join
//!    structures (e.g. `select` as a single object → array of objects).
//! 3. **Expression/Operator** — normalize operator names (e.g. `=` → `eq`),
//!    expression type tags, predicate shapes, and data types.

use super::aliases;
use super::array;
use super::expr;
use super::join;
use super::operators;
use super::order;
use super::query;
use super::select;
use super::table;
use super::value;
use super::where_;

/// Run the full normalization pipeline on a JSON value.
///
/// Returns `true` if any changes were made.
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let mut changed = false;

    // Stage 1: Field name aliases.
    // Must run before structural stages so repairs see canonical names.
    changed |= aliases::normalize_field_names(val);

    // Stage 2: Structure normalization.
    // Order matters: array → select → table → where → join → query.
    changed |= array::normalize(val); // ensure arrays, remove nulls
    changed |= select::normalize(val); // string→object, inject type, remove invalid
    changed |= table::normalize(val); // string→object
    changed |= where_::normalize(val); // array→object, extract top-level fields, flat condition
    changed |= join::normalize(val); // string→object, infer missing, strip unknown
    changed |= query::normalize(val); // strip unknown top-level fields, wrap expr

    // Stage 3: Expression/Operator normalization.
    // Must run after structure so arrays are unwrapped and fields are
    // canonical before we normalize their contents.
    changed |= operators::normalize(val); // = → eq, != → ne, etc.
    changed |= value::normalize(val); // integer → int, varchar → string, etc.
    changed |= expr::normalize(val); // inject missing type tags, fix predicate shapes

    // Stage 4: Order-by normalization (after aliases, structure, and expr).
    changed |= order::normalize(val); // normalize order_by items

    // Stage 5: Recurse into CTE sub-queries.
    // CTE sub-plans are independent QueryPlan JSON objects that need
    // the full normalize pipeline applied to them.
    if let Some(obj) = val.as_object_mut()
        && let Some(ctes) = obj.get_mut("ctes").and_then(|v| v.as_array_mut())
    {
        for cte in ctes.iter_mut() {
            if let Some(query) = cte.get_mut("query") {
                changed |= normalize(query);
            }
        }
    }

    // Stage 6: Downgrade recursive CTE flags.
    // Small models sometimes set `recursive: true` on CTEs, which the
    // current builder/compiler does not support.  Downgrade to `false`
    // with a warning so the plan can still compile (as non-recursive).
    if let Some(obj) = val.as_object_mut()
        && let Some(ctes) = obj.get_mut("ctes").and_then(|v| v.as_array_mut())
    {
        for cte in ctes.iter_mut() {
            if let Some(recursive) = cte.get_mut("recursive")
                && recursive.as_bool() == Some(true)
            {
                *recursive = serde_json::Value::Bool(false);
                changed = true;
            }
        }
    }

    changed
}

/// Selects a model-specific normalize pipeline.
///
/// Runs the full standard normalization pipeline first, then applies
/// model-specific extra normalizations for models known to produce
/// less-structured output (e.g. small models <3B params).
#[must_use]
pub fn normalize_for_model(raw: &mut serde_json::Value, model_fingerprint: Option<&str>) -> bool {
    let base = normalize(raw);

    let Some(fp) = model_fingerprint else {
        return base;
    };

    if is_small_model(fp) {
        normalize_small_model(raw) || base
    } else {
        base
    }
}

/// Check whether a model fingerprint identifies a small model (<3B params)
/// that typically produces less-structured output.
fn is_small_model(fp: &str) -> bool {
    let fp_lower = fp.to_lowercase();
    fp_lower.contains("llama-3.2")
        || fp_lower.contains("qwen2.5")
        || fp_lower.contains("phi-3")
        || fp_lower.contains("deepseek-coder")
        || fp_lower.contains("gemma-2")
        || fp_lower.contains("tiny")
}

/// Fix missing `type` discriminators on select-projection objects.
///
/// Injects a reasonable default `type` field for select items that lack one:
/// - `"column_ref"` if `column` or `columns` keys are present,
/// - `"expr"` if `expression` or `expr` keys are present,
/// - `"star"` otherwise.
///
/// Returns `true` if any change was made.
fn fix_select_types(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut changed = false;

    let select = match obj.get_mut("select").and_then(|v| v.as_array_mut()) {
        Some(arr) => arr,
        None => return false,
    };

    for proj in select.iter_mut() {
        let Some(proj_obj) = proj.as_object_mut() else {
            continue;
        };
        if proj_obj.contains_key("type") {
            continue;
        }
        if proj_obj.contains_key("column") || proj_obj.contains_key("columns") {
            proj_obj.insert(
                "type".to_owned(),
                serde_json::Value::String("column_ref".to_owned()),
            );
        } else if proj_obj.contains_key("expression") || proj_obj.contains_key("expr") {
            proj_obj.insert(
                "type".to_owned(),
                serde_json::Value::String("expr".to_owned()),
            );
        } else {
            proj_obj.insert(
                "type".to_owned(),
                serde_json::Value::String("star".to_owned()),
            );
        }
        changed = true;
    }

    changed
}

/// Small-model-specific normalizations:
///
/// - Inject missing `type` fields in select projections
/// - Fix `"from": "table_name"` (string) → `"from": {"table": "table_name"}`
/// - Fix LIMIT/offset as string → number
/// - Fix WHERE with missing `type` discriminator on predicates
#[must_use]
fn normalize_small_model(raw: &mut serde_json::Value) -> bool {
    let mut changed = false;

    let obj = match raw.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    // 1. Ensure select items have type fields (existing logic).
    changed |= fix_select_types(obj);

    // 2. Fix `"from": "table_name"` (string) → `"from": {"table": "table_name"}`.
    {
        let table_name = obj
            .get("from")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        if let Some(name) = table_name {
            obj["from"] = serde_json::json!({"table": name});
            changed = true;
        }
    }

    // 3. Fix LIMIT/offset as string → number.
    for &field in &["limit", "offset"] {
        if let Some(v) = obj.get(field)
            && let Some(s) = v.as_str()
            && let Ok(n) = s.parse::<u64>()
        {
            obj[field] = serde_json::json!(n);
            changed = true;
        }
    }

    // 4. Fix WHERE with missing `type` discriminator on predicates.
    if let Some(where_val) = obj.get_mut("where")
        && let Some(where_obj) = where_val.as_object_mut()
        && !where_obj.contains_key("type")
        && where_obj.contains_key("left")
        && where_obj.contains_key("op")
    {
        where_obj.insert("type".to_owned(), "comparison".into());
        changed = true;
    }

    changed
}

/// Normalize a string value (convenience: parse → normalize → serialize).
///
/// Returns the normalized JSON string, or the original input if
/// parsing fails.
#[must_use]
pub fn normalize_str(json_text: &str) -> std::borrow::Cow<'_, str> {
    let mut value: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return std::borrow::Cow::Borrowed(json_text),
    };

    if normalize(&mut value) {
        std::borrow::Cow::Owned(
            serde_json::to_string(&value).unwrap_or_else(|_| json_text.to_owned()),
        )
    } else {
        std::borrow::Cow::Borrowed(json_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pipeline_full_normalize() {
        let mut val = json!({
            "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}},
            "projection": ["id", "name"],
            "source": "users",
            "sort": [{"expr": {"column": "name"}, "descending": true}]
        });
        assert!(normalize(&mut val));
        // Stage 1: aliases
        assert!(val.get("where").is_some());
        assert!(val.get("select").is_some());
        assert!(val.get("from").is_some());
        assert!(val.get("order_by").is_some());
        // Stage 2: structures
        assert!(val.get("select").unwrap().is_array());
        assert!(val.get("from").unwrap().is_object());
        // select items should be objects with type
        let select = val.get("select").unwrap().as_array().unwrap();
        for item in select {
            assert!(
                item.get("type").is_some(),
                "each select item should have type"
            );
        }
    }

    #[test]
    fn pipeline_normalizes_case_when_expression() {
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
            "from": {"table": "orders"},
            "where": {
                "type": "and",
                "left": {"type": "is_null", "expr": {"type": "column_ref", "column": "user_id", "table": "users"}},
                "right": {"type": "comparison", "left": {"type": "column_ref", "column": "id", "table": "orders"}, "op": "=", "right": {"type": "column_ref", "column": "id", "table": "users"}}
            },
            "joins": [{"join_type": "inner", "right_table": {"table": "users"}, "on": {"type": "comparison", "left": {"type": "column_ref", "column": "user_id", "table": "orders"}, "op": "=", "right": {"type": "column_ref", "column": "id", "table": "users"}}}],
            "group_by": [{"type": "column_ref", "table": "orders", "column": "id"}]
        });
        assert!(normalize(&mut val));
        let select = val.get("select").unwrap().as_array().unwrap();
        let item2 = select[1].as_object().unwrap();
        // select[1] must still be type: "expr" after full pipeline
        assert_eq!(
            item2.get("type").and_then(|t| t.as_str()),
            Some("expr"),
            "select[1] type should be 'expr', not {:?}",
            item2.get("type")
        );
        // expression must be type: "case"
        if let Some(expr) = item2.get("expression").and_then(|e| e.as_object()) {
            assert_eq!(
                expr.get("type").and_then(|t| t.as_str()),
                Some("case"),
                "expression type should be 'case'"
            );
        } else {
            panic!("select[1] must have expression field as object");
        }
    }

    #[test]
    fn pipeline_applies_all_stages() {
        let mut val = json!({
            "projection": [{"type": "star"}],
            "source": "orders",
            "filter": [
                {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}, "limit": 10}
            ],
            "right": {"column": "id"},
            "expr": {"column": "name"},
            "descending": true
        });
        assert!(normalize(&mut val));
        // Aliases
        assert!(val.get("select").is_some());
        assert!(val.get("from").is_some());
        assert!(val.get("where").is_some());
        // Structure: where array → object
        assert!(val.get("where").unwrap().is_object());
        // Structure: limit extracted from where to top-level
        assert_eq!(val.get("limit").and_then(|v| v.as_u64()), Some(10));
        // Structure: from string → object
        assert!(val.get("from").unwrap().is_object());
        // Structure: query strips unknown fields
        assert!(val.get("right").is_none());
        // Structure: expr + descending → order_by
        assert!(val.get("order_by").is_some());
        assert!(val.get("expr").is_none());
        assert!(val.get("descending").is_none());
    }

    #[test]
    fn pipeline_no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {
                "type": "comparison",
                "left": {"type": "column_ref", "column": "age"},
                "op": "gt",
                "right": {"type": "literal", "value": 18, "data_type": "int"}
            },
        });
        assert!(!normalize(&mut val));
    }

    #[test]
    fn pipeline_normalize_str_owned() {
        let input =
            r#"{"filter": {"type": "comparison"}, "projection": ["id"], "source": "users"}"#;
        let result = normalize_str(input);
        assert!(result.contains(r#""where""#));
        assert!(result.contains(r#""select""#));
        assert!(result.contains(r#""from""#));
        assert!(!result.contains(r#""filter""#));
        assert!(!result.contains(r#""projection""#));
        assert!(!result.contains(r#""source""#));
    }

    #[test]
    fn pipeline_normalize_str_borrowed() {
        let input = r#"{"where": {"type": "comparison"}}"#;
        let result = normalize_str(input);
        if let std::borrow::Cow::Borrowed(s) = &result {
            assert_eq!(*s, input);
        } else {
            panic!("expected borrowed for unchanged input");
        }
    }

    #[test]
    fn pipeline_normalize_str_invalid_json() {
        let input = "not json at all";
        let result = normalize_str(input);
        assert_eq!(result.as_ref(), input);
    }

    // ── small-model tests ──────────────────────────────────────────────

    #[test]
    fn small_model_fixes_select_types() {
        let mut val = json!({"select": [{"column": "id"}], "from": {"table": "users"}});
        assert!(normalize_small_model(&mut val));
        assert_eq!(val["select"][0]["type"], "column_ref");
    }

    #[test]
    fn small_model_fixes_from_string() {
        let mut val = json!({"select": [{"column": "id"}], "from": "users"});
        assert!(normalize_small_model(&mut val));
        assert_eq!(val["from"]["table"], "users");
    }

    #[test]
    fn small_model_fixes_limit_string() {
        let mut val =
            json!({"select": [{"column": "id"}], "from": {"table": "users"}, "limit": "10"});
        assert!(normalize_small_model(&mut val));
        assert_eq!(val["limit"], 10);
    }

    #[test]
    fn small_model_fixes_where_missing_type() {
        let mut val = json!({"select": [{"column": "id"}], "from": {"table": "users"}, "where": {"left": {"column": "age"}, "op": "gt", "right": {"literal": 18}}});
        assert!(normalize_small_model(&mut val));
        assert_eq!(val["where"]["type"], "comparison");
    }

    #[test]
    fn small_model_no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "column_ref", "column": "id"}],
            "from": {"table": "users"},
            "where": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"literal": 18}},
            "limit": 10
        });
        assert!(!normalize_small_model(&mut val));
    }

    #[test]
    fn is_small_model_detects_small_models() {
        assert!(is_small_model("llama-3.2-3b-instruct"));
        assert!(is_small_model("qwen2.5-1.5b"));
        assert!(is_small_model("phi-3-mini"));
        assert!(is_small_model("deepseek-coder-1.3b"));
        assert!(is_small_model("gemma-2-2b"));
        assert!(is_small_model("tiny-llama"));
        assert!(!is_small_model("llama-3.1-8b"));
        assert!(!is_small_model("gpt-4"));
    }

    #[test]
    fn normalize_for_model_runs_small_pipeline_for_small_models() {
        let mut val = json!({"select": [{"column": "id"}], "from": "users", "limit": "5"});
        assert!(normalize_for_model(&mut val, Some("llama-3.2-3b")));
        assert_eq!(val["from"]["table"], "users");
        assert_eq!(val["limit"], 5);
    }
}
