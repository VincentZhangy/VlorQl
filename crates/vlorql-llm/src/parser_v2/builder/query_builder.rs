//! QueryPlan builder: canonical JSON → [`QueryPlan`](vlorql_core::schema::QueryPlan).
//!
//! Orchestrates all sub-builders to construct a complete `QueryPlan`
//! from canonical JSON.  This layer does **no** repair — it assumes
//! the input has already been normalized.

use serde_json::Value;
use vlorql_core::schema::{
    CommonTableExpression, OrderByTerm, Predicate, QueryPlan, SetOperation, SetOperationClause,
};

use super::expr_builder::{
    BuildError, build_expression, build_predicate, req_arr, req_obj, req_str,
};
use super::join_builder::build_join_clause;
use super::select_builder::build_projections;
use super::table_builder::build_from_clause;

/// Build a [`QueryPlan`](vlorql_core::schema::QueryPlan) from a canonical JSON value.
///
/// The input must be a JSON object with the standard QueryPlan fields.
/// All fields must already be in canonical form (normalized by the
/// normalize pipeline).
///
/// # Examples
///
/// ```
/// use vlorql_llm::parser_v2::builder::query_builder::build_plan;
/// use serde_json::json;
///
/// let json = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
/// let plan = build_plan(&json).unwrap();
/// assert_eq!(plan.from.table_name().unwrap(), "users");
/// ```
pub fn build_plan(value: &Value) -> Result<QueryPlan, BuildError> {
    let obj = req_obj(value, "plan")?;
    build_plan_from_obj(obj)
}

/// Extract an optional predicate field from a JSON object.
fn optional_predicate(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<Predicate>, BuildError> {
    obj.get(field)
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .map(|v| build_predicate(v).map_err(|e| e.at(field)))
        .transpose()
}

/// Extract and build an array field from a JSON object.
fn build_array_field<T>(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    builder: fn(&serde_json::Value) -> Result<T, BuildError>,
) -> Result<Option<Vec<T>>, BuildError> {
    obj.get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| builder(v).map_err(|e| e.at(&format!("{field}[{i}]"))))
                .collect::<Result<Vec<_>, _>>()
        })
        .filter(|v: &Result<Vec<_>, _>| !v.as_ref().is_ok_and(|x| x.is_empty()))
        .transpose()
}

/// Build a [`QueryPlan`](vlorql_core::schema::QueryPlan) from a canonical JSON object map.
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

    let r#where = optional_predicate(obj, "where")?;
    let group_by = build_array_field(obj, "group_by", build_expression)?;
    let having = optional_predicate(obj, "having")?;
    let order_by = build_array_field(obj, "order_by", build_order_by_term)?;

    let limit = obj.get("limit").and_then(|v| v.as_u64());
    let offset = obj.get("offset").and_then(|v| v.as_u64());

    let joins = build_array_field(obj, "joins", build_join_clause)?;
    let ctes = build_array_field(obj, "ctes", build_cte)?;

    // SELECT DISTINCT / DISTINCT ON
    let distinct = obj
        .get("distinct")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let distinct_on = match obj.get("distinct_on") {
        Some(v) if !v.is_null() => {
            let arr = req_arr(v, "distinct_on")?;
            let exprs: Result<Vec<_>, _> = arr
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    build_expression(item).map_err(|e| e.at(&format!("distinct_on[{i}]")))
                })
                .collect();
            Some(exprs?)
        }
        _ => None,
    };

    // Set operation (UNION / INTERSECT / EXCEPT) combining this query with another.
    let set_operation = match obj.get("set_operation") {
        Some(v) if !v.is_null() => Some(build_set_operation(v)?),
        _ => None,
    };

    Ok(QueryPlan {
        select,
        distinct,
        distinct_on,
        from,
        r#where,
        group_by,
        having,
        order_by,
        limit,
        offset,
        joins,
        ctes,
        set_operation,
    })
}

/// Build a [`SetOperationClause`] from a canonical JSON object.
///
/// Expected shape:
/// ```json
/// {"operation": "union_all", "right": {...}}
/// ```
///
/// Operation aliases are resolved by [`parse_set_operation`].
fn build_set_operation(val: &Value) -> Result<SetOperationClause, BuildError> {
    let obj = req_obj(val, "set_operation")?;
    let op_str = req_str(obj, "operation", "operation")?;
    let operation = parse_set_operation(op_str)?;
    let right_val = obj
        .get("right")
        .ok_or_else(|| BuildError::new("right", "missing `right` field on set_operation"))?;
    let right_obj = req_obj(right_val, "right")?;
    let right = Box::new(build_plan_from_obj(right_obj)?);
    Ok(SetOperationClause { operation, right })
}

/// Parse a set operation string, accepting common LLM spellings.
fn parse_set_operation(s: &str) -> Result<SetOperation, BuildError> {
    use SetOperation::*;
    match s {
        "union_all" | "union all" | "unionall" => Ok(UnionAll),
        "union" => Ok(Union),
        "intersect" => Ok(Intersect),
        "except" => Ok(Except),
        _ => Err(BuildError::new(
            "operation",
            format!("unknown set operation `{s}`"),
        )),
    }
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
    let recursive = obj
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let query_obj = req_obj(
        obj.get("query")
            .ok_or_else(|| BuildError::new("query", "missing `query` field on CTE"))?,
        "query",
    )?;
    let query = Box::new(build_plan_from_obj(query_obj)?);
    Ok(CommonTableExpression {
        name,
        query,
        recursive,
    })
}

/// Build a [`QueryPlan`](vlorql_core::schema::QueryPlan) from a canonical JSON string.
pub fn from_canonical_str(canonical: &str) -> Result<QueryPlan, serde_json::Error> {
    let value: Value = serde_json::from_str(canonical)?;
    build_plan(&value).map_err(Into::into)
}

/// Build a [`QueryPlan`](vlorql_core::schema::QueryPlan) from a canonical [`Value`].
pub fn from_canonical_value(canonical: &Value) -> Result<QueryPlan, serde_json::Error> {
    build_plan(canonical).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vlorql_core::schema::FromClause;

    #[test]
    fn build_plan_minimal() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"}
        });
        let plan = build_plan(&val).unwrap();
        assert_eq!(plan.select.len(), 1);
        assert_eq!(plan.from.table_name().unwrap(), "users");
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
        assert_eq!(plan.from.table_name().unwrap(), "users");
        assert_eq!(plan.from.alias().as_deref(), Some("u"));
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
        assert_eq!(plan.from.table_name().unwrap(), "users");
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
        assert_eq!(plan.from.table_name().unwrap(), "users");
    }

    #[test]
    fn from_canonical_value_roundtrip() {
        let val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        let plan = from_canonical_value(&val).unwrap();
        assert_eq!(plan.from.table_name().unwrap(), "users");
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

    #[test]
    fn build_plan_distinct_true() {
        let val = json!({"select": [{"type": "column_ref", "column": "status"}], "from": {"table": "users"}, "distinct": true});
        let plan = build_plan(&val).unwrap();
        assert!(plan.distinct, "distinct=true must propagate to QueryPlan");
    }

    #[test]
    fn build_plan_distinct_defaults_false() {
        let val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        let plan = build_plan(&val).unwrap();
        assert!(!plan.distinct, "missing distinct must default to false");
    }

    #[test]
    fn build_plan_distinct_on_pg() {
        let val = json!({
            "select": [{"type": "column_ref", "column": "id"}],
            "from": {"table": "users"},
            "distinct": true,
            "distinct_on": [{"type": "column_ref", "column": "name"}]
        });
        let plan = build_plan(&val).unwrap();
        assert!(plan.distinct);
        let on = plan.distinct_on.expect("distinct_on must be read");
        assert_eq!(on.len(), 1);
    }

    #[test]
    fn build_plan_union_all() {
        let val = json!({
            "select": [{"type": "column_ref", "column": "id"}],
            "from": {"table": "users"},
            "set_operation": {
                "operation": "union_all",
                "right": {"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "archived_users"}}
            }
        });
        let plan = build_plan(&val).unwrap();
        let set_op = plan.set_operation.expect("set_operation must be read");
        assert!(matches!(set_op.operation, SetOperation::UnionAll));
        assert_eq!(set_op.right.from.table_name().unwrap(), "archived_users");
    }

    #[test]
    fn build_plan_union_alias() {
        // "union" (without _all) → SetOperation::Union
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "union", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        let plan = build_plan(&val).unwrap();
        let set_op = plan.set_operation.unwrap();
        assert!(matches!(set_op.operation, SetOperation::Union));
    }

    #[test]
    fn build_plan_unknown_set_operation_fails() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "a"},
            "set_operation": {"operation": "merge", "right": {"select": [{"type": "star"}], "from": {"table": "b"}}}
        });
        let result = build_plan(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("set operation"));
    }

    #[test]
    fn build_plan_recursive_cte() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "ancestors"},
            "ctes": [{
                "name": "ancestors",
                "recursive": true,
                "query": {"select": [{"type": "star"}], "from": {"table": "tree"}}
            }]
        });
        let plan = build_plan(&val).unwrap();
        let ctes = plan.ctes.expect("ctes must be read");
        assert_eq!(ctes.len(), 1);
        assert!(ctes[0].recursive, "recursive=true must propagate to CTE");
    }

    #[test]
    fn build_plan_from_subquery() {
        let val = json!({
            "select": [{"type": "star"}],
            "from": {"type": "subquery", "query": {"select": [{"type": "column_ref", "column": "id"}], "from": {"table": "users"}}, "alias": "u"}
        });
        let plan = build_plan(&val).unwrap();
        match plan.from {
            FromClause::Subquery { alias, .. } => assert_eq!(alias.as_deref(), Some("u")),
            other => panic!("expected Subquery, got {other:?}"),
        }
    }
}
