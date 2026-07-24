//! QueryPlan builder: canonical JSON → [`QueryPlan`].
//!
//! Orchestrates all sub-builders to construct a complete `QueryPlan`
//! from canonical JSON.  This layer does **no** repair — it assumes
//! the input has already been normalized.

use serde_json::Value;
use vlorql_core::schema::{CommonTableExpression, OrderByTerm, QueryPlan};

use super::expr_builder::{
    BuildError, build_expression, build_predicate, req_arr, req_obj, req_str,
};
use super::join_builder::build_join_clause;
use super::select_builder::build_projections;
use super::table_builder::build_from_clause;

/// Build a [`QueryPlan`] from a canonical JSON value.
///
/// The input must be a JSON object with the standard QueryPlan fields.
/// All fields must already be in canonical form (normalized by the
/// normalize pipeline).
pub fn build_plan(value: &Value) -> Result<QueryPlan, BuildError> {
    let obj = req_obj(value, "plan")?;
    build_plan_from_obj(obj)
}

/// Build a [`QueryPlan`] from a canonical JSON object map.
pub fn build_plan_from_obj(obj: &serde_json::Map<String, Value>) -> Result<QueryPlan, BuildError> {
    let _path = "";

    let select = {
        let arr = req_arr(
            obj.get("select")
                .ok_or_else(|| BuildError::new("select", "missing `select` field"))?,
            "select",
        )?;
        build_projections(arr)?
    };

    let from = build_from_clause(
        obj.get("from")
            .ok_or_else(|| BuildError::new("from", "missing `from` field"))?,
        "from",
    )?;

    let r#where = obj
        .get("where")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at("where")))
        .transpose()?;

    let group_by = obj
        .get("group_by")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_expression(v).map_err(|e| e.at(&format!("group_by[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()?;

    let having = obj
        .get("having")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at("having")))
        .transpose()?;

    let order_by = obj
        .get("order_by")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_order_by_term(v).map_err(|e| e.at(&format!("order_by[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()?;

    let limit = obj.get("limit").and_then(|v| v.as_u64());
    let offset = obj.get("offset").and_then(|v| v.as_u64());

    let joins = obj
        .get("joins")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_join_clause(v).map_err(|e| e.at(&format!("joins[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    let ctes = obj
        .get("ctes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| build_cte(v).map_err(|e| e.at(&format!("ctes[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    Ok(QueryPlan {
        select,
        distinct: false,
        distinct_on: None,
        from,
        r#where,
        group_by,
        having,
        order_by,
        limit,
        offset,
        joins,
        ctes,
        set_operation: None,
    })
}

/// Build an [`OrderByTerm`] from a canonical JSON object.
fn build_order_by_term(val: &Value) -> Result<OrderByTerm, BuildError> {
    let obj = req_obj(val, "order_by_term")?;
    let expr = build_expression(
        obj.get("expr")
            .ok_or_else(|| BuildError::new("expr", "missing `expr` field on order_by term"))?,
    )
    .map_err(|e| e.at("expr"))?;
    let descending = obj
        .get("descending")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(OrderByTerm { expr, descending })
}

/// Build a [`CommonTableExpression`] from a canonical JSON object.
fn build_cte(val: &Value) -> Result<CommonTableExpression, BuildError> {
    let obj = req_obj(val, "cte")?;
    let name = req_str(obj, "name", "name")?.to_owned();
    let query_obj = req_obj(
        obj.get("query")
            .ok_or_else(|| BuildError::new("query", "missing `query` field on CTE"))?,
        "query",
    )?;
    let query = Box::new(build_plan_from_obj(query_obj)?);
    Ok(CommonTableExpression { name, query, recursive: false })
}

/// Build a [`QueryPlan`] from a canonical JSON string.
pub fn from_canonical_str(canonical: &str) -> Result<QueryPlan, serde_json::Error> {
    let value: Value = serde_json::from_str(canonical)?;
    build_plan(&value).map_err(Into::into)
}

/// Build a [`QueryPlan`] from a canonical [`Value`].
pub fn from_canonical_value(canonical: &Value) -> Result<QueryPlan, serde_json::Error> {
    build_plan(canonical).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_plan_minimal() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        let plan = build_plan(&val).unwrap();
        assert_eq!(plan.select.len(), 1);
        assert_eq!(plan.from.table, "users");
        assert!(plan.r#where.is_none());
        assert!(plan.group_by.is_none());
        assert!(plan.order_by.is_none());
        assert!(plan.limit.is_none());
        assert!(plan.offset.is_none());
        assert!(plan.joins.is_none());
        assert!(plan.ctes.is_none());
    }

    #[test]
    fn build_plan_full() {
        let val = json!({
            "select": [{"type": "column_ref", "column": "id"}, {"type": "column_ref", "column": "name"}],
            "from": {"table": "users", "alias": "u"},
            "where": {"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}},
            "group_by": [{"type": "column_ref", "column": "status"}],
            "having": {"type": "comparison", "left": {"type": "column_ref", "column": "count"}, "op": "gt", "right": {"type": "literal", "value": 5, "data_type": "int"}},
            "order_by": [{"expr": {"type": "column_ref", "column": "name"}, "descending": true}],
            "limit": 10,
            "offset": 20,
            "joins": [{"join_type": "inner", "right_table": {"table": "orders"}, "on": {"type": "comparison", "left": {"type": "column_ref", "column": "user_id"}, "op": "eq", "right": {"type": "column_ref", "column": "id"}}}],
            "ctes": [{"name": "active_users", "query": {"select": [{"type": "star"}], "from": {"table": "users"}}}]
        });
        let plan = build_plan(&val).unwrap();
        assert_eq!(plan.select.len(), 2);
        assert_eq!(plan.from.table, "users");
        assert_eq!(plan.from.alias, Some("u".to_owned()));
        assert!(plan.r#where.is_some());
        assert_eq!(plan.group_by.unwrap().len(), 1);
        assert!(plan.having.is_some());
        assert_eq!(plan.order_by.unwrap().len(), 1);
        assert_eq!(plan.limit, Some(10));
        assert_eq!(plan.offset, Some(20));
        assert_eq!(plan.joins.unwrap().len(), 1);
        assert_eq!(plan.ctes.unwrap().len(), 1);
    }

    #[test]
    fn build_plan_minimal_select_from() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        let plan = build_plan(&val).unwrap();
        assert_eq!(plan.select.len(), 1);
        assert_eq!(plan.from.table, "users");
    }

    #[test]
    fn build_plan_allows_null_where() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": null
        });
        let plan = build_plan(&val).unwrap();
        assert!(plan.r#where.is_none());
    }

    #[test]
    fn build_plan_missing_select() {
        let val = json!({
            "from": {"table": "users"}
        });
        let result = build_plan(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("select"));
    }

    #[test]
    fn build_plan_missing_from() {
        let val = json!({
            "select": [{"type": "star"}]
        });
        let result = build_plan(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("from"));
    }

    #[test]
    fn from_canonical_str_roundtrip() {
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;
        let plan = from_canonical_str(input).unwrap();
        assert_eq!(plan.from.table, "users");
    }

    #[test]
    fn from_canonical_value_roundtrip() {
        let val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        let plan = from_canonical_value(&val).unwrap();
        assert_eq!(plan.from.table, "users");
    }

    #[test]
    fn build_plan_with_subquery() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "where": {"type": "exists", "query": {"select": [{"type": "star"}], "from": {"table": "orders"}}}
        });
        let plan = build_plan(&val).unwrap();
        assert!(plan.r#where.is_some());
    }

    #[test]
    fn build_plan_with_cte() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "active_users"},
            "ctes": [{"name": "active_users", "query": {"select": [{"type": "star"}], "from": {"table": "users"}, "where": {"type": "comparison", "left": {"column": "status"}, "op": "eq", "right": {"value": "active"}}}}]
        });
        let plan = build_plan(&val).unwrap();
        assert_eq!(plan.ctes.unwrap().len(), 1);
    }
}
