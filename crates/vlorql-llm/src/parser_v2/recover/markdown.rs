//! Markdown code fence recovery.
//!
//! Strips fenced code blocks (`````​``, `` ```json ``​``, `` ```sql ``​``, etc.)
//! from LLM output so that the inner JSON can be parsed.

/// Strips a markdown code fence from the start and end of `text`.
///
/// Handles:
/// - ` ```json ` / ` ``` ` with or without a closing fence
/// - Arbitrary language tags (`` ```sql ``, ````text`, etc.)
/// - Trailing text after the closing fence
///
/// Returns `None` when no opening fence is found or the content
/// between fences is empty.
#[must_use]
pub fn strip_markdown_fence(text: &str) -> Option<&str> {
    let text = text.trim();

    if !text.starts_with("```") {
        return None;
    }

    let after_fence = &text[3..];

    // Find the end of the language tag line (if any).
    let content_start = after_fence.find('\n')? + 1;
    let content = &after_fence[content_start..];

    // Find closing fence — prefer the last one in case of nesting.
    let end = content.rfind("```").unwrap_or(content.len());
    let inner = content[..end].trim_end();

    if inner.is_empty() { None } else { Some(inner) }
}

/// Returns `true` when the text is enclosed in a markdown code fence.
#[must_use]
pub fn is_fenced(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("```")
}

/// Returns the language tag from a fenced code block, if present.
///
/// ` ```json{...} `` ` → `Some("json")`
/// ` ``` `` → `None`
/// ` ```sql `` → `Some("sql")`
#[must_use]
pub fn fence_language(text: &str) -> Option<&str> {
    let text = text.trim();
    let after_fence = text.strip_prefix("```")?;
    let lang = after_fence.split('\n').next()?;
    if lang.is_empty() { None } else { Some(lang.trim()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fence_json_lang() {
        let input = "```json\n{\"a\":1}\n```";
        assert_eq!(strip_markdown_fence(input), Some("{\"a\":1}"));
    }

    #[test]
    fn strip_fence_json_lang_no_newline() {
        let input = "```json\n{\"a\":1}```";
        let result = strip_markdown_fence(input);
        assert_eq!(result, Some("{\"a\":1}"));
    }

    #[test]
    fn strip_fence_no_lang() {
        let input = "```\n{\"a\":1}\n```";
        assert_eq!(strip_markdown_fence(input), Some("{\"a\":1}"));
    }

    #[test]
    fn strip_fence_without_closing() {
        let input = "```json\n{\"a\":1}\n";
        assert_eq!(strip_markdown_fence(input), Some("{\"a\":1}"));
    }

    #[test]
    fn strip_fence_with_text_after() {
        let input = "```json\n{\"a\":1}\n```\nsome trailing text";
        assert_eq!(strip_markdown_fence(input), Some("{\"a\":1}"));
    }

    #[test]
    fn strip_fence_arbitrary_lang() {
        let input = "```sql\nSELECT * FROM users\n```";
        assert_eq!(strip_markdown_fence(input), Some("SELECT * FROM users"));
    }

    #[test]
    fn strip_fence_not_fenced() {
        let input = "just plain text";
        assert_eq!(strip_markdown_fence(input), None);
    }

    #[test]
    fn strip_fence_empty_inner() {
        let input = "```json\n```";
        assert_eq!(strip_markdown_fence(input), None);
    }

    #[test]
    fn is_fenced_true() {
        assert!(is_fenced("```json\n{}```"));
        assert!(is_fenced("```\n{}```"));
    }

    #[test]
    fn is_fenced_false() {
        assert!(!is_fenced("{}"));
        assert!(!is_fenced("plain text"));
    }

    #[test]
    fn fence_language_returns_tag() {
        assert_eq!(fence_language("```json\n{}```"), Some("json"));
        assert_eq!(fence_language("```sql\nSELECT"), Some("sql"));
    }

    #[test]
    fn fence_language_no_tag() {
        assert_eq!(fence_language("```\n{}```"), None);
    }

    #[test]
    fn fence_language_not_fenced() {
        assert_eq!(fence_language("{}"), None);
    }
}