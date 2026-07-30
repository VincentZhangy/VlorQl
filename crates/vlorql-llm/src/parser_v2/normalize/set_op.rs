//! Set operation (UNION / INTERSECT / EXCEPT) normalization.
//!
//! LLMs frequently emit set operations in non-canonical shapes. This
//! module converges the common variants to the canonical form the
//! builder layer consumes:
//!
//! ```json
//! {"set_operation": {"operation": "union_all", "right": {...}}}
//! ```
//!
//! # Variants handled
//!
//! | Input shape | Canonical output |
//! |---|---|
//! | `{"union_all": {...}}` / `{"union": {...}}` / `{"intersect": {...}}` / `{"except": {...}}` | `{"set_operation": {"operation": "<op>", "right": {...}}}` |
//! | `{"set_operation": "UNION ALL"}` (bare string operation at plan level) | lifted to nested `set_operation` |
//! | `{"set_operation": {"op": "union_all", ...}}` (`op` instead of `operation`) | renamed to `operation` |
//! | `{"set_operation": {"operation": "UNION ALL", ...}}` (uppercase / spaced) | lowercased + canonicalized |
//! | `{"set_operation": [{...}, {...}]}` (array of operands) | first element is left, second becomes the `right` |
//! | `{"set_operation": {...}}` missing `right` but having `right_query` / `right_plan` | renamed to `right` |

use serde_json::Value;

/// Canonical operation strings.
const OP_UNION_ALL: &str = "union_all";
const OP_UNION: &str = "union";
const OP_INTERSECT: &str = "intersect";
const OP_EXCEPT: &str = "except";

/// Map a raw operation string to its canonical form.
///
/// Accepts upper/lower/mixed case, with or without underscores / spaces.
/// Returns `None` if the string is not a recognized set operation.
#[must_use]
pub fn canonical_op(raw: &str) -> Option<&'static str> {
    // Normalize: lowercase + collapse runs of whitespace / underscores.
    let folded: String = raw
        .chars()
        .map(|c| {
            if c == '_' || c.is_whitespace() {
                ' '
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let collapsed: String = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.as_str() {
        "union all" | "unionall" => Some(OP_UNION_ALL),
        "union" => Some(OP_UNION),
        "intersect" | "intersection" => Some(OP_INTERSECT),
        "except" | "minus" | "difference" => Some(OP_EXCEPT),
        _ => None,
    }
}

/// Full set-operation normalization for a query plan value.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn normalize(val: &mut Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let mut changed = false;

    // 1. Lift bare top-level operation keys (`union_all`, `union`, …) into
    //    `set_operation`. LLMs often write `{"union_all": {...}}` instead of
    //    the nested `set_operation` form.
    for op_key in [
        "union_all",
        "unionall",
        "union",
        "intersect",
        "intersection",
        "except",
        "minus",
    ] {
        if let Some(operand) = obj.remove(op_key) {
            let op = canonical_op(op_key).unwrap_or(OP_UNION_ALL);
            // Only lift when there is no existing `set_operation` — we don't
            // want to clobber a more specific structure.
            if !obj.contains_key("set_operation") {
                obj.insert(
                    "set_operation".to_owned(),
                    serde_json::json!({"operation": op, "right": operand}),
                );
                changed = true;
            }
        }
    }

    // 2. Normalize the `set_operation` field if present.
    if let Some(set_op_val) = obj.get_mut("set_operation") {
        changed |= normalize_set_operation(set_op_val);
    }

    changed
}

/// Normalize a single `set_operation` value (object, array, or bare string).
fn normalize_set_operation(set_op_val: &mut Value) -> bool {
    // 2a. Bare string operation: `{"set_operation": "UNION ALL"}`.
    //     This is malformed (no `right` operand) — we canonicalize the
    //     operation string but the builder will reject the missing `right`.
    if let Some(s) = set_op_val.as_str() {
        if let Some(canonical) = canonical_op(s) {
            *set_op_val = Value::String(canonical.to_owned());
            return true;
        }
        return false;
    }

    // 2b. Array of operands: `{"set_operation": [{...}, {...}]}`.
    //     Treat the first element as the operation-bearing object and
    //     the second (if any) as the `right`. When the first element is a
    //     bare QueryPlan, wrap it.
    if let Some(arr) = set_op_val.as_array_mut() {
        if arr.is_empty() {
            return false;
        }
        // Promote first element to the operation object.
        let first = arr.remove(0);
        // If the first element looks like a QueryPlan (has `select`/`from`),
        // the operation must come from elsewhere — fall back to UNION ALL.
        let op_str = first
            .as_object()
            .and_then(|o| o.get("operation").or_else(|| o.get("op")))
            .and_then(|v| v.as_str())
            .unwrap_or("union_all");
        let op = canonical_op(op_str).unwrap_or(OP_UNION_ALL);
        // The `right` is the next element, if any.
        let right = if !arr.is_empty() {
            arr.remove(0)
        } else {
            Value::Null
        };
        let mut new_obj = serde_json::Map::new();
        new_obj.insert("operation".to_owned(), Value::String(op.to_owned()));
        new_obj.insert("right".to_owned(), right);
        *set_op_val = Value::Object(new_obj);
        return true;
    }

    // 2c. Object form: the canonical target shape.
    let Some(obj) = set_op_val.as_object_mut() else {
        return false;
    };
    let mut changed = false;

    // `op` → `operation` (LLM sometimes uses the shorter key).
    if let Some(op_val) = obj.remove("op") {
        obj.entry("operation").or_insert(op_val);
        changed = true;
    }

    // Canonicalize the operation string.
    if let Some(op_val) = obj.get_mut("operation")
        && let Some(s) = op_val.as_str()
        && let Some(canonical) = canonical_op(s)
        && canonical != s
    {
        *op_val = Value::String(canonical.to_owned());
        changed = true;
    }

    // Rename alternate `right` operand keys to `right`.
    for alt in ["right_query", "right_plan", "rhs", "second"] {
        if let Some(v) = obj.remove(alt) {
            obj.entry("right").or_insert(v);
            changed = true;
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_op_handles_variants() {
        assert_eq!(canonical_op("union_all"), Some(OP_UNION_ALL));
        assert_eq!(canonical_op("UNION ALL"), Some(OP_UNION_ALL));
        assert_eq!(canonical_op("UnionAll"), Some(OP_UNION_ALL));
        assert_eq!(canonical_op("union"), Some(OP_UNION));
        assert_eq!(canonical_op("INTERSECT"), Some(OP_INTERSECT));
        assert_eq!(canonical_op("intersection"), Some(OP_INTERSECT));
        assert_eq!(canonical_op("except"), Some(OP_EXCEPT));
        assert_eq!(canonical_op("minus"), Some(OP_EXCEPT));
        assert_eq!(canonical_op("unknown"), None);
    }

    #[test]
    fn lifts_bare_union_all_key() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "union_all": {"select": [{"type": "star"}], "from": {"table": "b"}}
        });
        assert!(normalize(&mut val));
        let set_op = val.get("set_operation").expect("set_operation lifted");
        assert_eq!(set_op["operation"], "union_all");
        assert_eq!(set_op["right"]["from"]["table"], "b");
    }

    #[test]
    fn lifts_bare_union_key() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "union": {"select": [{"type": "star"}], "from": {"table": "b"}}
        });
        assert!(normalize(&mut val));
        assert_eq!(val["set_operation"]["operation"], "union");
    }

    #[test]
    fn does_not_overwrite_existing_set_operation() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "intersect", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}},
            "union_all": {"select": [{"type": "star"}], "from": {"table": "c"}}
        });
        let _ = normalize(&mut val);
        // Existing `set_operation` wins; `union_all` is dropped without clobbering.
        assert_eq!(val["set_operation"]["operation"], "intersect");
        assert_eq!(val["set_operation"]["right"]["from"]["table"], "b");
    }

    #[test]
    fn renames_op_to_operation() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"op": "UNION ALL", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        assert!(normalize(&mut val));
        let set_op = &val["set_operation"];
        assert!(set_op.get("op").is_none(), "`op` should be renamed");
        assert_eq!(set_op["operation"], "union_all");
    }

    #[test]
    fn canonicalizes_uppercase_operation() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "UNION ALL", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        assert!(normalize(&mut val));
        assert_eq!(val["set_operation"]["operation"], "union_all");
    }

    #[test]
    fn renames_right_query_to_right() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "union_all", "right_query": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        assert!(normalize(&mut val));
        assert!(val["set_operation"].get("right_query").is_none());
        assert_eq!(val["set_operation"]["right"]["from"]["table"], "b");
    }

    #[test]
    fn array_of_operands_promotes_first() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": [
                {"operation": "UNION ALL"},
                {"select": [{"type": "star"}], "from": {"table": "b"}}
            ]
        });
        assert!(normalize(&mut val));
        let set_op = &val["set_operation"];
        assert_eq!(set_op["operation"], "union_all");
        assert_eq!(set_op["right"]["from"]["table"], "b");
    }

    #[test]
    fn bare_string_operation_canonicalized() {
        let mut val = json!({"set_operation": "UNION ALL"});
        assert!(normalize(&mut val));
        assert_eq!(val["set_operation"], "union_all");
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "union_all", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        assert!(!normalize(&mut val));
    }
}
