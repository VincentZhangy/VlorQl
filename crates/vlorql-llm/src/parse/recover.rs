//! JSON recovery from raw LLM text.
//!
//! This layer only restores a JSON *string* (or slice) from messy model
//! output. It does **not** understand QueryPlan semantics.

/// Attempts to extract valid JSON from an LLM response text.
///
/// Small local LLMs often wrap JSON in markdown fences or include
/// extra text before/after the JSON object. This function tries
/// increasingly lenient strategies to recover valid JSON:
///
/// 1. Return the text as-is if it is already valid JSON.
/// 2. Strip markdown code fences (`` ```json … ``` `` or `` ``` … ``` ``).
/// 3. Find the outermost `{…}` JSON object in the text.
///
/// If no strategy yields valid JSON, the original text is returned
/// unchanged so the caller can produce an accurate error message.
#[must_use]
pub fn extract_json_content(raw: &str) -> &str {
    let trimmed = raw.trim();

    // 1. Already valid JSON — fast path.
    if is_valid_json_value(trimmed) {
        return trimmed;
    }

    // 2. Strip markdown fences.
    let no_fence = strip_markdown_fence(trimmed);
    if let Some(cleaned) = no_fence {
        if is_valid_json_value(cleaned) {
            return cleaned;
        }
        // Fence contents may have leading/trailing text — try JSON extraction.
        if let Some(obj) = find_outermost_json_obj(cleaned) {
            return obj;
        }
    }

    // 3. Find first JSON object anywhere in the text.
    if let Some(obj) = find_outermost_json_obj(trimmed) {
        return obj;
    }

    trimmed
}

/// Returns `true` when `text` is a valid JSON value.
fn is_valid_json_value(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

/// Strips a markdown code fence from the start and end of `text`.
fn strip_markdown_fence(text: &str) -> Option<&str> {
    for prefix in &["```json\n", "```json", "```\n", "```"] {
        if let Some(after_open) = text.strip_prefix(prefix) {
            let after_open = after_open.trim_start();
            // Find the closing fence
            let end = if let Some(close_pos) = after_open.rfind("```") {
                close_pos
            } else {
                after_open.len()
            };
            let inner = after_open[..end].trim_end();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }
    None
}

/// Finds the outermost JSON object (`{…}`) in a string by tracking
/// brace depth, respecting string boundaries so that braces inside
/// strings are not counted.  Returns `None` when no balanced object
/// is found.
fn find_outermost_json_obj(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth: u32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, ch) in text[start..].char_indices() {
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
                '{' => depth = depth.checked_add(1)?,
                '}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(&text[start..=start + i]);
                    }
                }
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    None
}


/// Returns a descriptive error message when `content` contains raw
/// chat-template tokens (`<|im_start|>`, `<|im_end|>`), which indicate
/// the model did not understand the output format constraint.
#[must_use]
pub fn detect_template_leak(content: &str) -> Option<String> {
    let has_start = content.contains("<|im_start|>");
    let has_end = content.contains("<|im_end|>");
    if !has_start && !has_end {
        return None;
    }
    Some(format!(
        "Model returned raw chat-template tokens{}. \
         This typically means the model does not support the `format` \
         parameter with a full JSON Schema. \
         Try setting `strict_json_schema = false` in `extra` of your \
         LLM configuration, or use a model that supports structured output.",
        if has_start && has_end {
            " (`<|im_start|>`, `<|im_end|>`)"
        } else if has_start {
            " (`<|im_start|>`)"
        } else {
            " (`<|im_end|>`)"
        }
    ))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_content_passes_through_valid_json() {
        let valid = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;
        assert_eq!(extract_json_content(valid), valid);
    }

    #[test]
    fn extract_json_content_strips_markdown_fence() {
        let fenced = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_strips_fence_without_closing() {
        let fenced = "```json\n{\"a\":1}\n";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_strips_fence_with_text_after() {
        let fenced = "```json\n{\"a\":1}\n```\nsome trailing text";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_finds_outermost_object() {
        let with_prefix = "Here is the JSON: {\"a\":1} end";
        assert_eq!(extract_json_content(with_prefix), "{\"a\":1}");
    }

    #[test]
    fn extract_json_content_handles_nested_braces() {
        let nested = "text {\"outer\": {\"inner\": 1}} trailing";
        assert_eq!(extract_json_content(nested), "{\"outer\": {\"inner\": 1}}");
    }

    #[test]
    fn extract_json_content_returns_original_when_no_json_found() {
        let no_json = "this is not json at all";
        assert_eq!(extract_json_content(no_json), no_json);
    }

    #[test]
    fn extract_json_content_strips_fence_then_finds_object() {
        let messy = "```markdown\nSome text {\"key\": \"value\"}\n```";
        assert_eq!(extract_json_content(messy), "{\"key\": \"value\"}");
    }

    #[test]
    fn find_outermost_json_obj_is_string_aware() {
        let input = r#"{"outer":{"inner":"some {text with} braces"}}"#;
        let found = find_outermost_json_obj(input);
        assert!(found.is_some(), "should find balanced outer object");
        assert_eq!(found.unwrap(), input);

        let with_braces_in_string =
            r#"{"where":[{"type":"and"},"string with {braces}"],"extra":"value"}"#;
        let found2 = find_outermost_json_obj(with_braces_in_string);
        assert!(found2.is_some(), "should handle braces inside strings");
        let parsed: serde_json::Value = serde_json::from_str(found2.unwrap()).unwrap();
        assert_eq!(parsed.get("extra").and_then(|v| v.as_str()), Some("value"));
    }
}
