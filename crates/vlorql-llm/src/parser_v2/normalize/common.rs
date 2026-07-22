//! Common utilities for the normalize layer.
//!
//! Shared helpers used by multiple normalize sub-modules.

/// Returns `true` when the value is an empty JSON array `[]`.
#[must_use]
pub fn is_empty_array(v: &serde_json::Value) -> bool {
    v.as_array().map_or(false, |arr| arr.is_empty())
}

/// Returns `true` when the value is a JSON null or `Value::Null`.
#[must_use]
pub fn is_null(v: &serde_json::Value) -> bool {
    v.is_null()
}

/// Returns `true` when the value is null or empty (empty array or
/// empty object).
#[must_use]
pub fn is_null_or_empty(v: &serde_json::Value) -> bool {
    v.is_null()
        || v.as_array().map_or(false, |a| a.is_empty())
        || v.as_object().map_or(false, |o| o.is_empty())
}