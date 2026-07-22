//! FromClause / table builder: canonical JSON → [`FromClause`].

use serde_json::Value;
use vlorql_core::schema::FromClause;

use super::expr_builder::{BuildError, opt_str, req_obj, req_str};

/// Build a [`FromClause`] from a canonical JSON object.
///
/// Expected shape:
/// ```json
/// {"table": "users", "alias": "u"}
/// ```
pub fn build_from_clause(val: &Value, parent: &str) -> Result<FromClause, BuildError> {
    let obj = req_obj(val, parent)?;
    let table = req_str(obj, "table", parent)?.to_owned();
    let alias = opt_str(obj, "alias").map(|s| s.to_owned());
    Ok(FromClause { table, alias })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_from_clause_with_table() {
        let val = json!({"table": "users"});
        let from = build_from_clause(&val, "from").unwrap();
        assert_eq!(from.table, "users");
        assert!(from.alias.is_none());
    }

    #[test]
    fn build_from_clause_with_alias() {
        let val = json!({"table": "users", "alias": "u"});
        let from = build_from_clause(&val, "from").unwrap();
        assert_eq!(from.table, "users");
        assert_eq!(from.alias, Some("u".to_owned()));
    }

    #[test]
    fn build_from_clause_missing_table() {
        let val = json!({"alias": "u"});
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
