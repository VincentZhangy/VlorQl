//! JSON validity and extraction utilities.
//!
//! Low-level helpers that check whether a string is valid JSON, and
//! extract JSON objects from array-wrapped LLM output.

use std::borrow::Cow;

/// Returns `true` when `text` is a valid JSON value (object, array,
/// string, number, boolean, or null).
#[must_use]
pub fn is_valid_json(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

/// Returns `true` when `text` is a valid JSON **object** (`{...}`).
#[must_use]
pub fn is_valid_json_object(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).map_or(false, |v| v.is_object())
}

/// Returns `true` when `text` is a valid JSON **array** (`[...]`).
#[must_use]
pub fn is_valid_json_array(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).map_or(false, |v| v.is_array())
}

/// If `text` is a JSON array whose first element is a JSON object,
/// returns that object as a string slice.  Returns `None` otherwise.
///
/// This handles the case where an LLM wraps the query plan in an
/// array: `[ { ... } ]` → `{ ... }`.
#[must_use]
pub fn extract_first_json_obj_from_array(text: &str) -> Option<&str> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = value.as_array()?;
    let first = arr.first()?;
    if first.is_object() {
        // We parsed the array to verify the first element is an object,
        // then re-find the braces in the original text to return a slice.
        if first.is_object() {
            return super::bracket::find_outermost_json_obj(text);
        }
    }
    None
}

/// Attempts to parse `text` as a JSON value.  Returns `None` on
/// parse failure.
#[must_use]
pub fn try_parse(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

/// Fixes LLM output where JSON keys/values are incorrectly escaped
/// with backslash-quote (`\"`) instead of plain quote (`"`).
///
/// Some models (especially small local models) produce `\"` as literal
/// characters in the output — e.g. `"where\":{\"type\":\"not\"` instead
/// of `"where":{"type":"not"`. This replacement makes the JSON parseable.
///
/// `\"` inside string values (e.g. `"hello \"world\""`) is also affected,
/// but QueryPlan JSON only contains simple identifiers, so this is safe.
#[must_use]
pub fn fix_escaped_quotes<'a>(text: &'a str) -> Cow<'a, str> {
    if !text.contains("\\\"") {
        return Cow::Borrowed(text);
    }
    // Only replace outside of already-valid JSON strings.
    // Strategy: try to parse; if it already works, return as-is.
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("\\\"", "\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_json_object() {
        assert!(is_valid_json(r#"{"a":1}"#));
        assert!(is_valid_json_object(r#"{"a":1}"#));
    }

    #[test]
    fn valid_json_array() {
        assert!(is_valid_json(r#"[1,2,3]"#));
        assert!(is_valid_json_array(r#"[1,2,3]"#));
        assert!(!is_valid_json_object(r#"[1,2,3]"#));
    }

    #[test]
    fn valid_json_scalar() {
        assert!(is_valid_json(r#""hello""#));
        assert!(is_valid_json(r#"42"#));
        assert!(is_valid_json(r#"true"#));
        assert!(is_valid_json(r#"null"#));
    }

    #[test]
    fn invalid_json() {
        assert!(!is_valid_json(r#"{a:1}"#));
        assert!(!is_valid_json(r#"not json"#));
    }

    #[test]
    fn extract_first_obj_from_array() {
        let input = r#"[{"a":1}]"#;
        assert_eq!(extract_first_json_obj_from_array(input), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extract_first_obj_from_array_multi() {
        let input = r#"[{"a":1},{"b":2}]"#;
        assert_eq!(extract_first_json_obj_from_array(input), Some(r#"{"a":1}"#));
    }

    #[test]
    fn extract_first_obj_from_non_array() {
        let input = r#"{"a":1}"#;
        assert_eq!(extract_first_json_obj_from_array(input), None);
    }

    #[test]
    fn extract_first_obj_from_array_non_object() {
        let input = r#"[42]"#;
        assert_eq!(extract_first_json_obj_from_array(input), None);
    }

    #[test]
    fn extract_first_obj_from_empty_array() {
        let input = r#"[]"#;
        assert_eq!(extract_first_json_obj_from_array(input), None);
    }

    #[test]
    fn try_parse_works() {
        assert!(try_parse(r#"{"a":1}"#).is_some());
        assert!(try_parse(r#"not json"#).is_none());
    }
}
