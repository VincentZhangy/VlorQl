//! FromClause / table builder: canonical JSON → [`FromClause`].

use serde_json::Value;
use vlorql_core::schema::{FromClause, QueryPlan};

use super::expr_builder::{BuildError, opt_str, req_obj};
use super::query_builder::build_plan_from_obj;

/// Build a [`FromClause`] from a canonical JSON object.
///
/// Recognized shapes:
/// - `{"table": "users", "alias": "u"}` — plain table reference
/// - `{"type": "subquery", "query": {...}, "alias": "t"}` — derived table
///   (`FROM (SELECT ...) AS t`)
/// - `{"type": "table", "table": "users", "alias": "u"}` — explicit `type`
///   discriminator emitted by some LLMs
///
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::table_builder::build_from_clause;
/// use serde_json::json;
///
/// let from = build_from_clause(&json!({"table": "users"}), "from").unwrap();
/// assert_eq!(from.table_name().unwrap(), "users");
/// ```
pub fn build_from_clause(val: &Value, parent: &str) -> Result<FromClause, BuildError> {
    let obj = req_obj(val, parent)?;
    let type_str = obj.get("type").and_then(|t| t.as_str());

    // Subquery: {"type": "subquery", "query": {...}, "alias": "..."}
    if type_str == Some("subquery") || type_str == Some("SubQuery") {
        let query_obj = req_obj(
            obj.get("query")
                .ok_or_else(|| BuildError::new("query", "missing `query` field on subquery"))?,
            "query",
        )?;
        let query: QueryPlan = build_plan_from_obj(query_obj)?;
        let alias = opt_str(obj, "alias").map(|s| s.to_owned());
        return Ok(FromClause::Subquery {
            query: Box::new(query),
            alias,
        });
    }

    // Table (with or without explicit `type: "table"` discriminator).
    // Fallback: if `table` is null (LLM bug), try `alias` as table name.
    let table = match obj.get("table").and_then(|v| v.as_str()) {
        Some(name) => name.to_owned(),
        None => {
            // Try alias as fallback table name.
            if let Some(alias) = opt_str(obj, "alias") {
                alias.to_owned()
            } else {
                return Err(BuildError::new(
                    parent,
                    "expected string `table`, got null (and no `alias` fallback)",
                ));
            }
        }
    };
    let alias = opt_str(obj, "alias").map(|s| s.to_owned());
    Ok(FromClause::table(table, alias))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_from_clause_with_table() {
        let val = json!({"table": "users"});
        let from = build_from_clause(&val, "from").unwrap();
        assert_eq!(from.table_name().unwrap(), "users");
        assert!(from.alias().is_none());
    }

    #[test]
    fn build_from_clause_with_alias() {
        let val = json!({"table": "users", "alias": "u"});
        let from = build_from_clause(&val, "from").unwrap();
        assert_eq!(from.table_name().unwrap(), "users");
        assert_eq!(from.alias(), Some("u".to_owned()));
    }

    #[test]
    fn build_from_clause_missing_table_falls_back_to_alias() {
        // When `table` is absent, `alias` is now used as the table name.
        let val = json!({"alias": "u"});
        let from = build_from_clause(&val, "from").unwrap();
        assert_eq!(from.table_name().unwrap(), "u");
        assert_eq!(from.alias(), Some("u".to_owned()));
    }

    #[test]
    fn build_from_clause_null_table_and_alias_errors() {
        let val = json!({"table": null, "alias": null});
        let result = build_from_clause(&val, "from");
        assert!(result.is_err());
    }

    #[test]
    fn build_from_clause_wrong_type() {
        let val = json!("users");
        let result = build_from_clause(&val, "from");
        assert!(result.is_err());
    }
}
