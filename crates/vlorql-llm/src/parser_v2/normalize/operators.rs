//! Operator name canonicalization.
//!
//! Normalizes operator names used by various LLMs to the canonical
//! form expected by the builder layer (`ComparisonOperator` /
//! `BinaryOperator`).
//!
//! # Examples
//!
//! - `=` / `==` / `equals` → `eq`
//! - `!=` / `<>` / `neq` / `not_equal` → `ne`
//! - `>` / `greater_than` → `gt`
//! - `>=` / `gte` / `greater_than_or_equal` → `gte`
//! - `+` / `add` → `add`

use serde_json::Value;

/// Comparison operator aliases: non-standard → canonical.
const COMPARISON_OPS: &[(&str, &str)] = &[
    ("=", "eq"),
    ("==", "eq"),
    ("equals", "eq"),
    ("equal", "eq"),
    ("!=", "ne"),
    ("<>", "ne"),
    ("neq", "ne"),
    ("ne", "ne"),
    ("not_equal", "ne"),
    ("not_equals", "ne"),
    (">", "gt"),
    ("greater_than", "gt"),
    (">=", "gte"),
    ("gte", "gte"),
    ("greater_than_or_equal", "gte"),
    ("greater_than_or_equals", "gte"),
    ("<", "lt"),
    ("less_than", "lt"),
    ("<=", "lte"),
    ("lte", "lte"),
    ("less_than_or_equal", "lte"),
    ("less_than_or_equals", "lte"),
];

/// Binary operator aliases: non-standard → canonical.
const BINARY_OPS: &[(&str, &str)] = &[
    ("+", "add"),
    ("plus", "add"),
    ("-", "sub"),
    ("minus", "sub"),
    ("subtract", "sub"),
    ("*", "mul"),
    ("multiply", "mul"),
    ("times", "mul"),
    ("/", "div"),
    ("divide", "div"),
    ("%", "mod"),
    ("modulo", "mod"),
];

/// Normalize all operator values (`op` field) in a JSON value tree.
///
/// Returns `true` if any operator was changed.
#[must_use]
pub fn normalize(val: &mut Value) -> bool {
    normalize_impl(val, "op")
}

/// Normalize operator values in a JSON value tree, looking at the
/// specified field name (e.g. `"op"` or `"operator"`).
///
/// Returns `true` if any operator was changed.
fn normalize_impl(val: &mut Value, field: &str) -> bool {
    let mut changed = false;
    match val {
        Value::Object(map) => {
            // Normalize the `op` field if present.
            if let Some(op_val) = map.get_mut(field) {
                if let Some(s) = op_val.as_str() {
                    if let Some(canonical) =
                        resolve_comparison_op(s).or_else(|| resolve_binary_op(s))
                    {
                        if canonical != s {
                            *op_val = Value::String(canonical.to_owned());
                            changed = true;
                        }
                    }
                }
            }
            // Recurse into children.
            for (_key, v) in map.iter_mut() {
                changed |= normalize_impl(v, field);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                changed |= normalize_impl(v, field);
            }
        }
        _ => {}
    }
    changed
}

/// Resolve a comparison operator alias to its canonical form.
#[must_use]
pub fn resolve_comparison_op(op: &str) -> Option<&'static str> {
    COMPARISON_OPS
        .iter()
        .find(|(from, _)| *from == op)
        .map(|(_, to)| *to)
}

/// Resolve a binary operator alias to its canonical form.
#[must_use]
pub fn resolve_binary_op(op: &str) -> Option<&'static str> {
    BINARY_OPS
        .iter()
        .find(|(from, _)| *from == op)
        .map(|(_, to)| *to)
}

/// Returns `true` when `op` is already a canonical operator name.
#[must_use]
pub fn is_canonical(op: &str) -> bool {
    matches!(
        op,
        "eq" | "ne" | "gt" | "gte" | "lt" | "lte" | "add" | "sub" | "mul" | "div" | "mod"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_eq_operator() {
        let mut val = json!({"type": "comparison", "left": {"column": "age"}, "op": "=", "right": {"value": 18}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("eq"));
    }

    #[test]
    fn normalizes_equals_string() {
        let mut val = json!({"type": "comparison", "op": "equals", "left": {"column": "status"}, "right": {"value": "active"}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("eq"));
    }

    #[test]
    fn normalizes_not_equal() {
        let mut val = json!({"type": "comparison", "op": "!=", "left": {"column": "status"}, "right": {"value": "deleted"}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("ne"));
    }

    #[test]
    fn normalizes_greater_than() {
        let mut val = json!({"type": "comparison", "op": "greater_than", "left": {"column": "price"}, "right": {"value": 100}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("gt"));
    }

    #[test]
    fn normalizes_less_than_or_equal() {
        let mut val = json!({"type": "comparison", "op": "<=", "left": {"column": "age"}, "right": {"value": 18}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("lte"));
    }

    #[test]
    fn normalizes_binary_ops() {
        let mut val = json!({"type": "binary_op", "op": "+", "left": {"column": "a"}, "right": {"column": "b"}});
        assert!(normalize(&mut val));
        assert_eq!(val.get("op").and_then(|v| v.as_str()), Some("add"));
    }

    #[test]
    fn normalizes_nested_operators() {
        let mut val = json!({
            "type": "and",
            "left": {"type": "comparison", "op": "=", "left": {"column": "a"}, "right": {"value": 1}},
            "right": {"type": "comparison", "op": ">", "left": {"column": "b"}, "right": {"value": 2}}
        });
        assert!(normalize(&mut val));
        assert_eq!(val.pointer("/left/op").and_then(|v| v.as_str()), Some("eq"));
        assert_eq!(
            val.pointer("/right/op").and_then(|v| v.as_str()),
            Some("gt")
        );
    }

    #[test]
    fn already_canonical_no_change() {
        let mut val = json!({"type": "comparison", "op": "eq", "left": {"column": "age"}, "right": {"value": 18}});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn no_op_field_no_change() {
        let mut val = json!({"type": "star"});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn resolve_comparison_ops() {
        assert_eq!(resolve_comparison_op("="), Some("eq"));
        assert_eq!(resolve_comparison_op("!="), Some("ne"));
        assert_eq!(resolve_comparison_op(">"), Some("gt"));
        assert_eq!(resolve_comparison_op(">="), Some("gte"));
        assert_eq!(resolve_comparison_op("<"), Some("lt"));
        assert_eq!(resolve_comparison_op("<="), Some("lte"));
        assert_eq!(resolve_comparison_op("greater_than"), Some("gt"));
        assert_eq!(resolve_comparison_op("eq"), None); // already canonical
        assert_eq!(resolve_comparison_op("unknown"), None);
    }

    #[test]
    fn resolve_binary_ops() {
        assert_eq!(resolve_binary_op("+"), Some("add"));
        assert_eq!(resolve_binary_op("-"), Some("sub"));
        assert_eq!(resolve_binary_op("*"), Some("mul"));
        assert_eq!(resolve_binary_op("/"), Some("div"));
        assert_eq!(resolve_binary_op("%"), Some("mod"));
        assert_eq!(resolve_binary_op("add"), None); // already canonical
    }

    #[test]
    fn is_canonical_works() {
        assert!(is_canonical("eq"));
        assert!(is_canonical("add"));
        assert!(!is_canonical("="));
        assert!(!is_canonical("equals"));
    }

    #[test]
    fn normalize_recursively_through_array() {
        let mut val = json!([
            {"type": "comparison", "op": "=", "left": {"column": "a"}, "right": {"value": 1}},
            {"type": "comparison", "op": ">", "left": {"column": "b"}, "right": {"value": 2}}
        ]);
        assert!(normalize(&mut val));
        assert_eq!(val[0].get("op").and_then(|v| v.as_str()), Some("eq"));
        assert_eq!(val[1].get("op").and_then(|v| v.as_str()), Some("gt"));
    }
}
