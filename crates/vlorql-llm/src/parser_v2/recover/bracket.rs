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

/// Finds the "best" balanced JSON object in `text`.
///
/// Unlike [`find_outermost_json_obj`], which returns the first balanced
/// `{…}`, this scans **every** `{` start position, keeps only candidates
/// that parse as JSON, and returns the best one: objects that look like a
/// query plan (contain a `select` or `from` key) win over those that
/// don't, and among equals the longest wins. This tolerates models that
/// emit reasoning prose (possibly containing braces) before the plan, or
/// multiple JSON objects.
///
/// Returns `None` if no substring parses as a JSON object.
#[must_use]
pub fn find_best_json_obj(text: &str) -> Option<&str> {
    let mut best: Option<&str> = None;
    let mut best_score = (false, 0usize); // (looks_like_plan, byte_len)
    let mut idx = 0;
    while let Some(rel) = text[idx..].find('{') {
        let start = idx + rel;
        if let Some(end) = find_matching_close(&text[start..], '{', '}') {
            let candidate = &text[start..=start + end];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
                let looks_like_plan = v.get("select").is_some() || v.get("from").is_some();
                let score = (looks_like_plan, candidate.len());
                if best.is_none() || score > best_score {
                    best = Some(candidate);
                    best_score = score;
                }
            }
        }
        idx = start + 1; // '{' is ASCII → safe byte boundary
    }
    best
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

/// Finds the end index of a balanced JSON object starting at `start`.
///
/// `text` must have a `{` at byte position `start`. Returns the byte
/// index of the matching `}`, or `None` if no match is found.
#[must_use]
pub(crate) fn find_balanced_object_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let rel_end = find_matching_close(rest, '{', '}')?;
    Some(start + rel_end)
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
    find_outermost_json_obj(text).is_some_and(|found| found.len() == text.len())
}

/// Attempt to repair a truncated JSON string by appending missing
/// closing braces (`}`) and brackets (`]`) based on the current
/// brace depth.  Returns the repaired string, or the original if
/// it is already valid JSON or cannot be repaired.
#[must_use]
pub fn repair_truncated_json(json: &str) -> std::borrow::Cow<'_, str> {
    // Fast path: already valid JSON.
    if serde_json::from_str::<serde_json::Value>(json).is_ok() {
        return std::borrow::Cow::Borrowed(json);
    }
    // Only attempt repair if the input looks like the start of an object.
    let trimmed = json.trim();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return std::borrow::Cow::Borrowed(json);
    }
    // Count brace/bracket depth, respecting strings.
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
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
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    if depth <= 0 {
        return std::borrow::Cow::Borrowed(json);
    }
    // Append missing closing characters (last opened first).
    let mut repaired = trimmed.to_owned();
    // Re-scan to determine the order of missing closers.
    let mut stack: Vec<char> = Vec::new();
    in_string = false;
    escaped = false;
    for ch in trimmed.chars() {
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
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    stack.pop();
                }
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    while let Some(closer) = stack.pop() {
        repaired.push(closer);
    }
    std::borrow::Cow::Owned(repaired)
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

    #[test]
    fn find_best_json_obj_skips_leading_prose_braces() {
        let input = r#"Here is my reasoning {note: skip me} and the plan:
        {"select":[{"type":"star"}],"from":{"table":"users"}}"#;
        let found = find_best_json_obj(input).expect("should find the plan object");
        let v: serde_json::Value = serde_json::from_str(found).unwrap();
        assert!(
            v.get("select").is_some(),
            "should pick the object with select/from, got: {found}"
        );
    }

    #[test]
    fn find_best_json_obj_prefers_plan_over_first() {
        let input = r#"{"error":"none"} {"select":[{"type":"star"}],"from":{"table":"t"}}"#;
        let found = find_best_json_obj(input).unwrap();
        let v: serde_json::Value = serde_json::from_str(found).unwrap();
        assert!(v.get("from").is_some());
    }

    #[test]
    fn find_best_json_obj_none_when_no_valid_json() {
        assert_eq!(find_best_json_obj("no json { unbalanced"), None);
    }
}
