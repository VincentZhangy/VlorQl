//! Common utilities for the normalize layer.
//!
//! Shared helpers used by multiple normalize sub-modules.

use serde_json::Value;

/// Data type aliases: non-standard → canonical serde form.
const DATA_TYPE_ALIASES: &[(&str, &str)] = &[
    // Integer types
    ("int2", "int"),
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
    // Decimal types
    ("decimal", "decimal"),
    ("numeric", "decimal"),
    // Float types
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
    // Blob types
    ("bytea", "blob"),
    // Null variants
    ("NULL", "null"),
    ("Null", "null"),
    // JSON types
    ("jsonb", "json"),
];

/// Maps a raw type tag plus its JSON value to the canonical
/// `data_type` string. The ambiguous `"number"` tag is disambiguated by
/// inspecting whether the value is integral, so both normalization paths
/// agree on `int` vs `float`.
#[must_use]
pub fn canonical_data_type(dt: &str, value: Option<&Value>) -> Option<&'static str> {
    if dt == "number" {
        return Some(match value.and_then(|v| v.as_f64()) {
            Some(f) if f.fract() == 0.0 && f.is_finite() => "int",
            Some(_) => "float",
            None => "int",
        });
    }
    match dt {
        "string" => Some("string"),
        "integer" => Some("int"),
        "double" | "real" => Some("float"),
        "boolean" | "bool" => Some("boolean"),
        "null" => Some("null"),
        "decimal" => Some("decimal"),
        _ => None,
    }
}

/// Resolve a data type alias to its canonical form.
///
/// Returns `None` if the type is already canonical or unknown.
#[must_use]
pub fn resolve_sql_type_alias(dt: &str) -> Option<&'static str> {
    DATA_TYPE_ALIASES
        .iter()
        .find(|(from, _)| *from == dt)
        .map(|(_, to)| *to)
}

/// Returns `true` when the value is an empty JSON array `[]`.
#[must_use]
pub fn is_empty_array(v: &Value) -> bool {
    v.as_array().is_some_and(|arr| arr.is_empty())
}

/// Returns `true` when the value is a JSON null or `Value::Null`.
#[must_use]
pub fn is_null(v: &Value) -> bool {
    v.is_null()
}

/// Returns `true` when the value is null or empty (empty array or
/// empty object).
#[must_use]
pub fn is_null_or_empty(v: &Value) -> bool {
    v.is_null()
        || v.as_array().is_some_and(|a| a.is_empty())
        || v.as_object().is_some_and(|o| o.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_data_type_string() {
        assert_eq!(canonical_data_type("string", None), Some("string"));
    }
    #[test]
    fn canonical_data_type_integer() {
        assert_eq!(canonical_data_type("integer", None), Some("int"));
    }
    #[test]
    fn canonical_data_type_number_int_value() {
        assert_eq!(canonical_data_type("number", Some(&json!(42))), Some("int"));
    }
    #[test]
    fn canonical_data_type_number_float_value() {
        assert_eq!(
            canonical_data_type("number", Some(&json!(3.14))),
            Some("float")
        );
    }
    #[test]
    fn canonical_data_type_already_canonical() {
        assert_eq!(canonical_data_type("int", None), None);
        assert_eq!(canonical_data_type("float", None), None);
    }
    #[test]
    fn resolve_sql_type_alias_works() {
        assert_eq!(resolve_sql_type_alias("integer"), Some("int"));
        assert_eq!(resolve_sql_type_alias("varchar"), Some("string"));
        assert_eq!(resolve_sql_type_alias("decimal"), Some("decimal"));
        assert_eq!(resolve_sql_type_alias("int"), None);
    }

    #[test]
    fn canonical_data_type_decimal() {
        assert_eq!(canonical_data_type("decimal", None), Some("decimal"));
    }
}
