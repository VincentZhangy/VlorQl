//! CASE expression normalization.
//!
//! Converts malformed `function_call(name: "case")` with interleaved
//! WHEN/THEN args to a proper `case` expression structure.
//! All items are `pub(super)` — visible within the `expr` module but
//! not outside it.

use serde_json::Value;

/// Convert a malformed `function_call(name: "case")` to a proper `case` expression.
///
/// Operates on the inner Map to avoid borrow-checker conflicts with the
/// `Value::Object(map)` destructuring in `normalize_impl`.
#[must_use]
pub(super) fn normalize_malformed_case_expression_in_map(
    map: &mut serde_json::Map<String, Value>,
) -> bool {
    let args = match map.remove("args").and_then(|a| a.as_array().cloned()) {
        Some(a) => a,
        None => return false,
    };

    let mut when_thens = Vec::new();
    let mut else_result = Value::Null;
    let mut i = 0;
    while i < args.len() {
        let current = &args[i];
        if current.is_null() {
            i += 1;
            break;
        }
        if i + 1 >= args.len() {
            else_result = current.clone();
            break;
        }
        let next = &args[i + 1];
        if next.is_null() {
            i += 2;
            continue;
        }
        let mut when = current.clone();
        // `comparison` is a Predicate type, not an Expression.
        // Convert it to `binary_op` so the expression builder can
        // handle it inside `CASE WHEN`.
        if let Some(when_obj) = when.as_object_mut()
            && when_obj.get("type").and_then(|t| t.as_str()) == Some("comparison")
        {
            when_obj.insert("type".to_owned(), Value::String("binary_op".to_owned()));
        }
        when_thens.push(serde_json::json!({
            "when": when,
            "then": next.clone(),
        }));
        i += 2;
    }

    if i < args.len() && !args[i].is_null() {
        else_result = args[i].clone();
    }

    map.clear();
    map.insert("type".to_owned(), Value::String("case".to_owned()));
    map.insert("operand".to_owned(), Value::Null);
    map.insert("when_thens".to_owned(), Value::Array(when_thens));
    map.insert("else_result".to_owned(), else_result);

    true
}
