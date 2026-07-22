//! FROM clause structure normalization.
//!
//! Ensures `from` is always an object with a `table` field, even when
//! the LLM emits a bare string.

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
    string_to_object(val)
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
