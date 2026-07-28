//! FROM clause structure normalization.
//!
//! Ensures `from` is always an object with a `table` field, even when
//! the LLM emits a bare string.

use serde_json::Value;

/// Convert a bare string `from` to a `{"table": "..."}` object.
///
/// Returns `true` if any change was made.
#[must_use]
pub fn string_to_object(val: &mut serde_json::Value) -> bool {
    let Some(obj) = val.as_object_mut() else {
        return false;
    };
    let Some(from_val) = obj.get("from") else {
        return false;
    };

    if let Some(table_name) = from_val.as_str() {
        obj.insert("from".to_owned(), serde_json::json!({"table": table_name}));
        return true;
    }

    false
}

/// Full FROM structure normalization.
#[must_use]
pub fn normalize(val: &mut serde_json::Value) -> bool {
    let changed = string_to_object(val);
    changed | array_to_object(val)
}

/// When the LLM outputs `"from"` as an array (e.g. `"from":[{"type":"join",...},{"table":"t"}]`),
/// extract the first valid table reference and promote any join-like elements to the `joins` field.
///
/// Returns `true` if any change was made.
#[must_use]
fn array_to_object(val: &mut serde_json::Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let from_arr = match obj.get("from").and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a.clone(),
        _ => return false,
    };

    // Find the first table-like element and collect join-like elements
    let mut from_obj = None;
    let mut joins = Vec::new();

    for elem in &from_arr {
        if let Some(e) = elem.as_object() {
            if e.contains_key("table") {
                // This is a table reference — use the first one as `from`
                if from_obj.is_none() {
                    from_obj = Some(elem.clone());
                }
            } else if e.contains_key("join_type") || e.get("type").and_then(|t| t.as_str()) == Some("join") {
                // This is a join specification — promote to joins list
                let join_entry = serde_json::json!({
                    "join_type": e.get("join_type").or(e.get("type")).and_then(|v| v.as_str()).unwrap_or("inner"),
                    "right_table": e.get("right_table").cloned().unwrap_or(Value::Null),
                    "on": e.get("on").cloned().unwrap_or(Value::Null),
                });
                joins.push(join_entry);
            }
        }
    }

    if let Some(from) = from_obj {
        obj.insert("from".to_owned(), from);
        if !joins.is_empty() {
            // Merge existing joins with extracted joins
            let existing = obj.get("joins").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let merged = [existing, joins].concat();
            obj.insert("joins".to_owned(), Value::Array(merged));
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_string_to_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": "users"});
        assert!(string_to_object(&mut val));
        let from = val.get("from").unwrap();
        assert_eq!(from.get("table").and_then(|v| v.as_str()), Some("users"));
    }

    #[test]
    fn from_already_object() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(!string_to_object(&mut val));
    }

    #[test]
    fn from_missing() {
        let mut val = json!({"select": [{"type": "star"}]});
        assert!(!string_to_object(&mut val));
    }

    #[test]
    fn normalize_works() {
        let mut val = json!({"select": [{"type": "star"}], "from": "orders"});
        assert!(normalize(&mut val));
        assert_eq!(
            val.get("from")
                .and_then(|v| v.get("table"))
                .and_then(|v| v.as_str()),
            Some("orders")
        );
    }

    #[test]
    fn no_change_for_canonical() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(!normalize(&mut val));
    }
}
