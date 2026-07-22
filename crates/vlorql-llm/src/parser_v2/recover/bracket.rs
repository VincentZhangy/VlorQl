//! Bracket-matching recovery utilities.
//!
//! Finds balanced brace/bracket pairs in raw text, respecting string
//! boundaries so that braces inside JSON strings are not counted.

/// Finds the outermost JSON object (`{…}`) in a string by tracking
/// brace depth, respecting string boundaries so that braces inside
/// strings are not counted.
///
/// Returns `None` when no balanced object is found.
#[must_use]
pub fn find_outermost_json_obj(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = find_matching_close(&text[start..], '{', '}')?;
    Some(&text[start..=start + end])
}

/// Finds the outermost array brackets (`[…]`) in a string by tracking
/// bracket depth, respecting string boundaries.
///
/// Returns `None` when no balanced array is found.
#[must_use]
pub fn find_outermost_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = find_matching_close(&text[start..], '[', ']')?;
    Some(&text[start..=start + end])
}

/// Finds the matching close delimiter for an open delimiter at the
/// start of `text`, respecting string boundaries.
///
/// Returns the index (relative to `text`) of the matching close
/// delimiter, or `None` if no match is found.
///
/// `text` is expected to start with `open`.
fn find_matching_close(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                c if c == open => depth = depth.checked_add(1)?,
                c if c == close => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    None
}

/// Strips whitespace from the start and end of text, then checks
/// whether the text starts with `{` and ends with `}` at the same
/// brace-depth level.
#[must_use]
pub fn is_balanced_object(text: &str) -> bool {
    let text = text.trim();
    if !text.starts_with('{') {
        return false;
    }
    find_outermost_json_obj(text).map_or(false, |found| found.len() == text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_outermost_json_obj_simple() {
        let input = r#"text {"a": 1} trailing"#;
        assert_eq!(find_outermost_json_obj(input), Some(r#"{"a": 1}"#));
    }

    #[test]
    fn find_outermost_json_obj_nested() {
        let input = r#"{"outer": {"inner": 1}}"#;
        assert_eq!(find_outermost_json_obj(input), Some(input));
    }

    #[test]
    fn find_outermost_json_obj_string_braces() {
        let input = r#"{"outer":{"inner":"some {text with} braces"}}"#;
        let found = find_outermost_json_obj(input);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), input);
    }

    #[test]
    fn find_outermost_json_obj_with_braces_in_string() {
        let input = r#"{"where":[{"type":"and"},"string with {braces}"],"extra":"value"}"#;
        let found = find_outermost_json_obj(input);
        assert!(found.is_some(), "should handle braces inside strings");
        let parsed: serde_json::Value = serde_json::from_str(found.unwrap()).unwrap();
        assert_eq!(parsed.get("extra").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn find_outermost_json_obj_no_brace() {
        let input = "no braces here";
        assert_eq!(find_outermost_json_obj(input), None);
    }

    #[test]
    fn find_outermost_json_obj_unbalanced() {
        let input = r#"{"a":1"#;
        assert_eq!(find_outermost_json_obj(input), None);
    }

    #[test]
    fn find_outermost_array_simple() {
        let input = r#"text [1, 2, 3] trailing"#;
        assert_eq!(find_outermost_array(input), Some(r#"[1, 2, 3]"#));
    }

    #[test]
    fn find_outermost_array_nested() {
        let input = r#"[[1, 2], [3, 4]]"#;
        assert_eq!(find_outermost_array(input), Some(input));
    }

    #[test]
    fn is_balanced_object_true() {
        assert!(is_balanced_object(r#"{"a":1}"#));
        assert!(is_balanced_object(r#"  {"a":1}  "#));
    }

    #[test]
    fn is_balanced_object_false() {
        assert!(!is_balanced_object(r#"{"a":1"#));
        assert!(!is_balanced_object(r#"not an object"#));
    }

    #[test]
    fn find_matching_close_basic() {
        let result = find_matching_close("{hello}", '{', '}');
        // "{hello}" — `{` at index 0, `}` at index 6 (0-indexed)
        assert_eq!(result, Some(6));
    }

    #[test]
    fn find_matching_close_string_aware() {
        let input = r#"{"key": "some {text}"}"#;
        let result = find_matching_close(input, '{', '}');
        assert!(result.is_some());
        // The closing brace should be the one after the string, not the one inside the string
        let end = result.unwrap();
        assert_eq!(&input[..=end], input);
    }
}
