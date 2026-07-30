//! V2 Pipeline: unified entry point for the full parsing pipeline.
//!
//! Runs the complete pipeline: recover → normalize → build → fix →
//! validate → optimize → return [`QueryPlan`](vlorql_core::schema::QueryPlan).
//!
//! This is the recommended public API for parsing LLM output into a
//! validated and optimized [`QueryPlan`](vlorql_core::schema::QueryPlan).

use crate::parser_v2::builder::query_builder;
use crate::parser_v2::fix::fixer;
use crate::parser_v2::normalize::pipeline as normalize_pipeline;
use crate::parser_v2::optimize::optimize as optimize_plan;
use crate::parser_v2::recover::bracket::repair_truncated_json;
use crate::parser_v2::recover::extract_json_content;
use crate::parser_v2::schema_check;
use crate::parser_v2::validate::validator;
use vlorql_core::schema::QueryPlan;

/// Error type for the V2 parsing pipeline.
#[derive(Debug)]
pub enum ParseError {
    /// The input could not be parsed as JSON.
    InvalidJson(String),
    /// The JSON could not be built into a QueryPlan.
    BuildError(String),
    /// The plan failed semantic validation.
    ValidationErrors(Vec<String>),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidJson(msg) => write!(f, "Invalid JSON: {}", msg),
            ParseError::BuildError(msg) => write!(f, "Build error: {}", msg),
            ParseError::ValidationErrors(errors) => {
                write!(f, "Validation errors: [{}]", errors.join(", "))
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Run the full V2 pipeline: raw LLM text → validated, optimized [`QueryPlan`](vlorql_core::schema::QueryPlan).
///
/// # Stages
///
/// 1. **Recover** — extract JSON from raw text (strip markdown fences, etc.)
/// 2. **Normalize** — canonicalize field names, structures, operators, expressions
/// 3. **Build** — canonical JSON → QueryPlan AST
/// 4. **Fix** — auto-fix safe defaults (missing aliases, limit zero, etc.)
/// 5. **Validate** — semantic validation
/// 6. **Optimize** — AST optimization (predicate simplification, projection pruning)
///
/// # Errors
///
/// Returns [`ParseError`] when the input cannot be parsed, built, or
/// validated.  The error message is human-readable.
///
/// # Examples
///
/// ```ignore
/// use vlorql_llm::parser_v2::parse_query_plan;
///
/// let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
/// let plan = parse_query_plan(raw, None).unwrap();
/// assert_eq!(plan.from.table_name().unwrap(), "users");
/// ```
pub fn parse_query_plan(raw: &str, model_name: Option<&str>) -> Result<QueryPlan, ParseError> {
    // Stage 1: Recover — extract JSON from raw text.
    let json_str = extract_json_content(raw);

    // Stage 1b: Repair — if JSON is truncated (missing closing braces),
    // append the required closing characters so serde can parse it.
    let json_str = repair_truncated_json(json_str);

    // Stage 2: Normalize — canonicalize the JSON (model-aware).
    let mut value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    let _ = normalize_pipeline::normalize_for_model(&mut value, model_name);

    // Stage 2b: Schema validation
    let schema_errors = schema_check::validate_against_schema(&value);
    if let Err(ref errors) = schema_errors {
        for err in errors {
            tracing::debug!("Schema validation: {err}");
        }
    }

    // Stage 3: Build — canonical JSON → QueryPlan AST.
    let mut plan =
        query_builder::build_plan(&value).map_err(|e| ParseError::BuildError(e.to_string()))?;

    // Stage 4: Fix — auto-fix safe defaults.
    let _ = fixer::fix_plan(&mut plan);

    // Stage 5: Validate — semantic validation.
    if let Err(errors) = validator::validate_plan(&plan) {
        let messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        return Err(ParseError::ValidationErrors(messages));
    }

    // Stage 6: Optimize — AST optimization.
    let _ = optimize_plan(&mut plan);

    Ok(plan)
}

/// Run the full pipeline but skip validation (for debugging / lenient mode).
///
/// Useful when you want to see the plan even if it has minor issues.
pub fn parse_query_plan_lenient(
    raw: &str,
    model_name: Option<&str>,
) -> Result<QueryPlan, ParseError> {
    let json_str = extract_json_content(raw);
    let json_str = repair_truncated_json(json_str);
    let mut value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    let _ = normalize_pipeline::normalize_for_model(&mut value, model_name);

    // Stage 2b: Schema validation
    let schema_errors = schema_check::validate_against_schema(&value);
    if let Err(ref errors) = schema_errors {
        for err in errors {
            tracing::debug!("Schema validation: {err}");
        }
    }

    let mut plan =
        query_builder::build_plan(&value).map_err(|e| ParseError::BuildError(e.to_string()))?;
    let _ = fixer::fix_plan(&mut plan);
    let _ = optimize_plan(&mut plan);
    Ok(plan)
}

/// Run the full pipeline with all intermediate results (for debugging).
pub struct ParseResult {
    /// The raw JSON string after recovery.
    pub json_str: String,
    /// The canonical JSON value after normalization.
    pub canonical: serde_json::Value,
    /// The final QueryPlan.
    pub plan: QueryPlan,
}

/// Parse with full debug output.
pub fn parse_query_plan_debug(
    raw: &str,
    model_name: Option<&str>,
) -> Result<ParseResult, ParseError> {
    let json_str = extract_json_content(raw).to_owned();
    let mut value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| ParseError::InvalidJson(e.to_string()))?;
    let _ = normalize_pipeline::normalize_for_model(&mut value, model_name);

    // Stage 2b: Schema validation
    let schema_errors = schema_check::validate_against_schema(&value);
    if let Err(ref errors) = schema_errors {
        for err in errors {
            tracing::debug!("Schema validation: {err}");
        }
    }

    let canonical = value.clone();
    let mut plan =
        query_builder::build_plan(&value).map_err(|e| ParseError::BuildError(e.to_string()))?;
    let _ = fixer::fix_plan(&mut plan);
    if let Err(errors) = validator::validate_plan(&plan) {
        let messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        return Err(ParseError::ValidationErrors(messages));
    }
    let _ = optimize_plan(&mut plan);
    Ok(ParseResult {
        json_str,
        canonical,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_star_plan() {
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "users");
        assert_eq!(plan.select.len(), 1);
    }

    #[test]
    fn parse_with_markdown_fence() {
        let raw =
            "```json\n{\"select\": [{\"type\": \"star\"}], \"from\": {\"table\": \"users\"}}\n```";
        let plan = parse_query_plan(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "users");
    }

    #[test]
    fn parse_deepseek_style() {
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "orders"}, "filter": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "orders");
        assert!(plan.r#where.is_some());
    }

    #[test]
    fn parse_qwen_style() {
        let raw = r#"{"projection": ["id", "name"], "source": "users", "filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "users");
        assert_eq!(plan.select.len(), 2);
        assert!(plan.r#where.is_some());
    }

    #[test]
    fn parse_llama_style() {
        let raw = "Here is the plan:\n```json\n{\"select\": [{\"type\": \"star\"}], \"from\": {\"table\": \"products\"}, \"where\": [{\"type\": \"comparison\", \"left\": {\"column\": \"price\"}, \"op\": \"lt\", \"right\": {\"value\": 100}}], \"sort\": [{\"expr\": {\"column\": \"name\"}, \"descending\": true}]}\n```";
        let plan = parse_query_plan(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "products");
        assert!(plan.r#where.is_some());
        assert!(plan.order_by.is_some());
    }

    #[test]
    fn parse_invalid_json() {
        let raw = "this is not json";
        let result = parse_query_plan(raw, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::InvalidJson(_)));
    }

    #[test]
    fn parse_missing_from() {
        let raw = r#"{"select": [{"type": "star"}]}"#;
        let result = parse_query_plan(raw, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::BuildError(_)));
    }

    #[test]
    fn parse_lenient_skips_validation() {
        // Lenient mode should still produce a plan even with limit=0
        // (which would normally fail validation).
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}, "limit": 0}"#;
        let plan = parse_query_plan_lenient(raw, None).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "users");
        // fixer removes limit=0
        assert_eq!(plan.limit, None);
    }

    #[test]
    fn parse_debug_returns_intermediates() {
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "users"}}"#;
        let result = parse_query_plan_debug(raw, None).unwrap();
        assert!(result.json_str.contains("select"));
        assert!(result.canonical.get("select").is_some());
        assert_eq!(result.plan.from.table_name().unwrap(), "users");
    }

    #[test]
    fn parse_distinct_propagates() {
        let raw = r#"{"select": [{"type": "column_ref", "column": "status"}], "from": {"table": "users"}, "distinct": true}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        assert!(plan.distinct, "distinct=true must propagate through the full pipeline");
    }

    #[test]
    fn parse_union_all_end_to_end() {
        let raw = r#"{"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}, "set_operation": {"operation": "union_all", "right": {"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "archived_users"}}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let set_op = plan.set_operation.expect("set_operation must propagate");
        assert!(matches!(set_op.operation, vlorql_core::schema::SetOperation::UnionAll));
        assert_eq!(set_op.right.from.table_name().unwrap(), "archived_users");
    }

    #[test]
    fn parse_recursive_cte_end_to_end() {
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "ancestors"}, "ctes": [{"name": "ancestors", "recursive": true, "query": {"select": [{"type": "star"}], "from": {"table": "tree"}}}]}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let ctes = plan.ctes.expect("ctes must propagate");
        assert_eq!(ctes.len(), 1);
        assert!(ctes[0].recursive, "recursive=true must propagate through the full pipeline");
    }

    #[test]
    fn parse_from_subquery_end_to_end() {
        let raw = r#"{"select": [{"type": "star"}], "from": {"type": "subquery", "query": {"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}}, "alias": "u"}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        match plan.from {
            vlorql_core::schema::FromClause::Subquery { alias, .. } => {
                assert_eq!(alias.as_deref(), Some("u"));
            }
            other => panic!("expected Subquery FromClause, got {other:?}"),
        }
    }

    #[test]
    fn parse_bare_union_all_lifted_end_to_end() {
        // LLM writes `{"union_all": {...}}` instead of nested set_operation.
        let raw = r#"{"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}, "union_all": {"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "archived_users"}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let set_op = plan.set_operation.expect("union_all must lift to set_operation");
        assert!(matches!(set_op.operation, vlorql_core::schema::SetOperation::UnionAll));
        assert_eq!(set_op.right.from.table_name().unwrap(), "archived_users");
    }

    #[test]
    fn parse_uppercase_union_all_end_to_end() {
        // LLM writes uppercase / spaced operation strings.
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "a"}, "set_operation": {"operation": "UNION ALL", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let set_op = plan.set_operation.unwrap();
        assert!(matches!(set_op.operation, vlorql_core::schema::SetOperation::UnionAll));
    }

    #[test]
    fn parse_op_field_renamed_end_to_end() {
        // LLM uses `op` instead of `operation`.
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "a"}, "set_operation": {"op": "intersect", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let set_op = plan.set_operation.unwrap();
        assert!(matches!(set_op.operation, vlorql_core::schema::SetOperation::Intersect));
    }

    #[test]
    fn parse_minus_alias_end_to_end() {
        // `minus` is the Oracle spelling of EXCEPT.
        let raw = r#"{"select": [{"type": "star"}], "from": {"table": "a"}, "minus": {"select": [{"type": "star"}], "from": {"table": "b"}}}"#;
        let plan = parse_query_plan(raw, None).unwrap();
        let set_op = plan.set_operation.unwrap();
        assert!(matches!(set_op.operation, vlorql_core::schema::SetOperation::Except));
    }
}
