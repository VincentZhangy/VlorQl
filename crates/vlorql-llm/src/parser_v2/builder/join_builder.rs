//! Join clause builder: canonical JSON → [`JoinClause`].

use serde_json::Value;
use vlorql_core::schema::{JoinClause, Predicate};

use super::expr_builder::{BuildError, build_predicate, parse_join_type, req_obj, req_str};
use super::table_builder::build_from_clause;

/// Build a [`JoinClause`] from a canonical JSON object.
///
/// Expected shape:
/// ```json
/// {"join_type": "inner", "right_table": {"table": "orders"}, "on": {...}}
/// ```
///
/// For `CROSS JOIN`, the `on` field is optional (not used).
pub fn build_join_clause(val: &Value) -> Result<JoinClause, BuildError> {
    let obj = req_obj(val, "join")?;

    let type_str = req_str(obj, "join_type", "join_type")?;
    let join_type = parse_join_type(type_str)?;

    let right_table = build_from_clause(
        obj.get("right_table")
            .ok_or_else(|| BuildError::new("right_table", "missing `right_table` field on join"))?,
        "right_table",
    )?;

    let on = if let Some(on_val) = obj.get("on") {
        build_predicate(on_val).map_err(|e| e.at("on"))?
    } else if join_type == vlorql_core::schema::JoinType::Cross {
        // Cross joins don't need ON — provide a dummy TRUE predicate.
        Predicate::Comparison {
            left: vlorql_core::schema::Expression::Literal {
                value: serde_json::Value::Bool(true),
                data_type: vlorql_core::schema::DataType::Boolean,
            },
            op: vlorql_core::schema::ComparisonOperator::Eq,
            right: vlorql_core::schema::Expression::Literal {
                value: serde_json::Value::Bool(true),
                data_type: vlorql_core::schema::DataType::Boolean,
            },
        }
    } else {
        return Err(BuildError::new("on", "missing `on` field on join"));
    };

    Ok(JoinClause {
        join_type,
        right_table,
        on,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_inner_join() {
        let val = json!({"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"column": "user_id"}, "op": "eq", "right": {"column": "id"}}});
        let join = build_join_clause(&val).unwrap();
        assert_eq!(join.right_table.table, "orders");
    }

    #[test]
    fn build_left_join() {
        let val = json!({"join_type": "left", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"column": "user_id"}, "op": "eq", "right": {"column": "id"}}});
        let join = build_join_clause(&val).unwrap();
        assert_eq!(join.right_table.table, "orders");
    }

    #[test]
    fn build_cross_join_without_on() {
        let val = json!({"join_type": "cross", "right_table": {"table": "orders"}});
        let join = build_join_clause(&val).unwrap();
        assert_eq!(join.right_table.table, "orders");
    }

    #[test]
    fn build_cross_join_with_on() {
        let val = json!({"join_type": "cross", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"column": "user_id"}, "op": "eq", "right": {"column": "id"}}});
        let join = build_join_clause(&val).unwrap();
        assert_eq!(join.right_table.table, "orders");
    }

    #[test]
    fn error_on_missing_join_type() {
        let val = json!({"right_table": {"table": "orders"}, "on": {"type": "comparison"}});
        let result = build_join_clause(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_missing_right_table() {
        let val = json!({"join_type": "inner", "on": {"type": "comparison"}});
        let result = build_join_clause(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_missing_on_for_non_cross() {
        let val = json!({"join_type": "inner", "right_table": {"table": "orders"}});
        let result = build_join_clause(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_unknown_join_type() {
        let val = json!({"join_type": "unknown", "right_table": {"table": "orders"}, "on": {"type": "comparison"}});
        let result = build_join_clause(&val);
        assert!(result.is_err());
    }
}
