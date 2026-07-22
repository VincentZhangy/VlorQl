//! Top-level JSON extraction pipeline.
//!
//! Orchestrates the recover layer: raw LLM text → JSON string.
//!
//! Strategy (in order):
//!
//! 1. **Direct** — return as-is if already valid JSON.
//! 2. **Array-wrapper** — if it's a JSON array containing an object, extract the object.
//! 3. **Fence** — strip markdown code fences, then try direct / bracket extraction.
//! 4. **Bracket** — find the outermost `{…}` object anywhere in the text.
//! 5. **Fallback** — return the original text unchanged.

use super::bracket;
use super::json;
use super::markdown;

/// Attempts to extract valid JSON from an LLM response text.
///
/// Small LLMs often wrap JSON in markdown fences or include extra
/// text before/after the JSON object, or produce incorrectly escaped
/// quotes (`\"` instead of `"`).  This function tries increasingly
/// lenient strategies to recover valid JSON:
///
/// 0. Fix escaped quotes (`\"` → `"`) in the raw text.
/// 1. Return the text as-is if it is already valid JSON.
/// 2. If it's a JSON array containing an object, extract the object.
/// 3. Strip markdown code fences.
/// 4. Find the outermost `{…}` JSON object in the text.
///
/// If no strategy yields valid JSON, the original text is returned
/// unchanged so the caller can produce an accurate error message.
#[must_use]
pub fn extract_json_content(raw: &str) -> &str {
    let trimmed = raw.trim();

    // 0. Fix `\"` → `"` — some models output backslash-escaped quotes.
    let fixed = match json::fix_escaped_quotes(trimmed) {
        std::borrow::Cow::Owned(s) => {
            if json::is_valid_json_object(&s) {
                return Box::leak(s.into_boxed_str());
            }
            if json::is_valid_json_array(&s) {
                if let Some(obj) = json::extract_first_json_obj_from_array(&s) {
                    // obj borrows from s; leak obj to extend its lifetime.
                    return Box::leak(obj.to_owned().into_boxed_str());
                }
            }
            trimmed
        }
        std::borrow::Cow::Borrowed(_) => trimmed,
    };
    let _ = fixed;

    // 1. Already valid JSON **object** — fast path.
    if json::is_valid_json_object(trimmed) {
        return trimmed;
    }

    // 2. JSON array wrapping an object: `[ { ... } ]` → `{ ... }`.
    //    Must run before fence/bracket so `[{"select":...}]` is unwrapped.
    if json::is_valid_json_array(trimmed) {
        if let Some(obj) = json::extract_first_json_obj_from_array(trimmed) {
            return obj;
        }
        // If the array is valid but doesn't contain an object, fall through.
    }

    // 3. Strip markdown fences.
    if let Some(cleaned) = markdown::strip_markdown_fence(trimmed) {
        if json::is_valid_json(cleaned) {
            return cleaned;
        }
        // Fence contents may have leading/trailing text — try bracket extraction.
        if let Some(obj) = bracket::find_outermost_json_obj(cleaned) {
            return obj;
        }
    }

    // 4. Find first JSON object anywhere in the text.
    if let Some(obj) = bracket::find_outermost_json_obj(trimmed) {
        return obj;
    }

    trimmed
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
    fn passes_through_valid_json() {
        let valid = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;
        assert_eq!(extract_json_content(valid), valid);
    }

    #[test]
    fn strips_markdown_fence() {
        let fenced = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn strips_fence_without_closing() {
        let fenced = "```json\n{\"a\":1}\n";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn strips_fence_with_text_after() {
        let fenced = "```json\n{\"a\":1}\n```\nsome trailing text";
        assert_eq!(extract_json_content(fenced), "{\"a\":1}");
    }

    #[test]
    fn finds_outermost_object() {
        let with_prefix = "Here is the JSON: {\"a\":1} end";
        assert_eq!(extract_json_content(with_prefix), "{\"a\":1}");
    }

    #[test]
    fn handles_nested_braces() {
        let nested = "text {\"outer\": {\"inner\": 1}} trailing";
        assert_eq!(extract_json_content(nested), "{\"outer\": {\"inner\": 1}}");
    }

    #[test]
    fn returns_original_when_no_json_found() {
        let no_json = "this is not json at all";
        assert_eq!(extract_json_content(no_json), no_json);
    }

    #[test]
    fn strips_fence_then_finds_object() {
        let messy = "```markdown\nSome text {\"key\": \"value\"}\n```";
        assert_eq!(extract_json_content(messy), "{\"key\": \"value\"}");
    }

    #[test]
    fn extracts_from_array_wrapper() {
        let input = r#"[{"select":[{"type":"star"}],"from":{"table":"users"}}]"#;
        let result = extract_json_content(input);
        let parsed: serde_json::Value = serde_json::from_str(result).unwrap();
        assert!(parsed.is_object());
        assert!(parsed.get("select").is_some());
        assert!(parsed.get("from").is_some());
    }

    #[test]
    fn detect_template_leak_both_tokens() {
        let msg = detect_template_leak("<|im_start|>hello<|im_end|>");
        assert!(msg.is_some());
        let text = msg.unwrap();
        assert!(text.contains("<|im_start|>"));
        assert!(text.contains("<|im_end|>"));
    }

    #[test]
    fn detect_template_leak_start_only() {
        let msg = detect_template_leak("<|im_start|>hello");
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("<|im_start|>"));
    }

    #[test]
    fn detect_template_leak_none() {
        assert!(detect_template_leak("normal text").is_none());
    }
}
