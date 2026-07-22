//! Value / literal normalization.
//!
//! Normalizes literal values and data types (e.g. `"integer"` → `"int"`,
//! `"varchar"` → `"string"`).

use serde_json::Value;

/// Data type aliases: non-standard → canonical serde form.
const DATA_TYPE_ALIASES: &[(&str, &str)] = &[
    // Integer types
    ("integer", "int"),
    ("int4", "int"),
    ("int8", "int"),
    ("bigint", "int"),
    ("smallint", "int"),
    ("tinyint", "int"),
    // String types
    ("varchar", "string"),
    ("text", "string"),
    ("char", "string"),
    ("character", "string"),
    ("character varying", "string"),
    // Float types
    ("decimal", "float"),
    ("numeric", "float"),
    ("real", "float"),
    ("double", "float"),
    ("double precision", "float"),
    // Boolean types
    ("bool", "boolean"),
    // Timestamp types
    ("timestampz", "timestamp"),
    ("timestamptz", "timestamp"),
    ("datetime", "timestamp"),
    ("timestamp with time zone", "timestamp"),
    ("timestamp without time zone", "timestamp"),
    ("date", "timestamp"),
    // Null variants
    ("NULL", "null"),
    ("Null", "null"),
];

/// Resolve a data type alias to its canonical form.
///
/// Returns `None` if the type is already canonical or unknown.
#[must_use]
pub fn resolve_data_type(dt: &str) -> Option<&'static str> {
    DATA_TYPE_ALIASES
        .iter()
        .find(|(from, _)| *from == dt)
        .map(|(_, to)| *to)
}

/// Normalize all `data_type` fields in a JSON value tree.
///
/// Returns `true` if any data_type was changed.
#[must_use]
pub fn normalize(val: &mut Value) -> bool {
    normalize_impl(val)
}

fn normalize_impl(val: &mut Value) -> bool {
    let mut changed = false;
    match val {
        Value::Object(map) => {
            // Normalize the `data_type` field if present.
            if let Some(dt_val) = map.get_mut("data_type") {
                if let Some(s) = dt_val.as_str() {
                    if let Some(canonical) = resolve_data_type(s) {
                        if canonical != s {
                            *dt_val = Value::String(canonical.to_owned());
                            changed = true;
                        }
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

/// Returns `true` when `dt` is already a canonical data type name.
#[must_use]
pub fn is_canonical(dt: &str) -> bool {
    matches!(dt, "int" | "string" | "float" | "boolean" | "timestamp" | "null" | "json" | "uuid" | "blob")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_integer_to_int() {
        let mut val = json!({"type": "literal", "value": 42, "data_type": "integer"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("int"));
    }

    #[test]
    fn normalizes_varchar_to_string() {
        let mut val = json!({"type": "literal", "value": "hello", "data_type": "varchar"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("string"));
    }

    #[test]
    fn normalizes_bigint_to_int() {
        let mut val = json!({"type": "literal", "value": 100, "data_type": "bigint"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("int"));
    }

    #[test]
    fn normalizes_decimal_to_float() {
        let mut val = json!({"type": "literal", "value": 3.14, "data_type": "decimal"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("float"));
    }

    #[test]
    fn normalizes_bool_to_boolean() {
        let mut val = json!({"type": "literal", "value": true, "data_type": "bool"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("boolean"));
    }

    #[test]
    fn normalizes_datetime_to_timestamp() {
        let mut val = json!({"type": "literal", "value": "2024-01-01", "data_type": "datetime"});
        assert!(normalize(&mut val));
        assert_eq!(val.get("data_type").and_then(|v| v.as_str()), Some("timestamp"));
    }

    #[test]
    fn normalizes_nested_data_types() {
        let mut val = json!({
            "type": "comparison",
            "left": {"type": "column_ref", "column": "price"},
            "op": "gt",
            "right": {"type": "literal", "value": 100, "data_type": "integer"}
        });
        assert!(normalize(&mut val));
        assert_eq!(
            val.pointer("/right/data_type").and_then(|v| v.as_str()),
            Some("int")
        );
    }

    #[test]
    fn already_canonical_no_change() {
        let mut val = json!({"type": "literal", "value": 42, "data_type": "int"});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn no_data_type_field_no_change() {
        let mut val = json!({"type": "star"});
        assert!(!normalize(&mut val));
    }

    #[test]
    fn resolve_data_type_works() {
        assert_eq!(resolve_data_type("integer"), Some("int"));
        assert_eq!(resolve_data_type("varchar"), Some("string"));
        assert_eq!(resolve_data_type("decimal"), Some("float"));
        assert_eq!(resolve_data_type("bool"), Some("boolean"));
        assert_eq!(resolve_data_type("datetime"), Some("timestamp"));
        assert_eq!(resolve_data_type("int"), None); // already canonical
        assert_eq!(resolve_data_type("unknown"), None);
    }

    #[test]
    fn is_canonical_works() {
        assert!(is_canonical("int"));
        assert!(is_canonical("string"));
        assert!(!is_canonical("integer"));
        assert!(!is_canonical("varchar"));
    }

    #[test]
    fn normalizes_through_array() {
        let mut val = json!([
            {"type": "literal", "value": 1, "data_type": "integer"},
            {"type": "literal", "value": "hello", "data_type": "varchar"}
        ]);
        assert!(normalize(&mut val));
        assert_eq!(val[0].get("data_type").and_then(|v| v.as_str()), Some("int"));
        assert_eq!(val[1].get("data_type").and_then(|v| v.as_str()), Some("string"));
    }
}