use super::{build_expression, build_predicate};
use super::helpers::{parse_data_type, parse_comparison_op, parse_binary_op, parse_join_type};
use serde_json::json;
use vlorql_core::schema::{Expression, Predicate, InTarget, BinaryOperator, DataType};
use vlorql_core::schema::ComparisonOperator::*;
use vlorql_core::schema::DataType::*;
use vlorql_core::schema::JoinType::*;

// ── Expression building ───────────────────────────────────────

#[test]
fn build_column_ref() {
    let val = json!({"type": "column_ref", "table": "users", "column": "id"});
    let expr = build_expression(&val).unwrap();
    assert!(
        matches!(expr, Expression::ColumnRef { table: Some(t), column: c } if t == "users" && c == "id")
    );
}

#[test]
fn build_column_ref_no_table() {
    let val = json!({"type": "column_ref", "column": "name"});
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::ColumnRef { table: None, column: c } if c == "name"));
}

#[test]
fn build_literal_from_object() {
    let val = json!({"type": "literal", "value": 42, "data_type": "int"});
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::Literal { value: v, data_type: Int } if v == json!(42)));
}

#[test]
fn build_literal_from_bare_number() {
    let val = json!(42);
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::Literal { value: v, data_type: Int } if v == json!(42)));
}

#[test]
fn build_literal_from_bare_string() {
    let val = json!("hello");
    let expr = build_expression(&val).unwrap();
    assert!(
        matches!(expr, Expression::Literal { value: v, data_type: DataType::String } if v == json!("hello"))
    );
}

#[test]
fn build_literal_from_bare_null() {
    let val = json!(null);
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::Literal { value: v, data_type: Null } if v.is_null()));
}

#[test]
fn build_function_call() {
    let val = json!({"type": "function_call", "name": "count", "args": [{"type": "star"}], "distinct": false});
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::FunctionCall { name, .. } if name == "count"));
}

#[test]
fn build_binary_op() {
    let val = json!({"type": "binary_op", "left": {"type": "column_ref", "column": "a"}, "op": "add", "right": {"type": "column_ref", "column": "b"}});
    let expr = build_expression(&val).unwrap();
    assert!(matches!(
        expr,
        Expression::BinaryOp {
            op: BinaryOperator::Add,
            ..
        }
    ));
}

#[test]
fn build_star() {
    let val = json!({"type": "star"});
    let expr = build_expression(&val).unwrap();
    assert!(matches!(expr, Expression::Star));
}

#[test]
fn build_infer_type_from_fields() {
    let val = json!({"column": "age", "table": "users"});
    let expr = build_expression(&val).unwrap();
    assert!(
        matches!(expr, Expression::ColumnRef { table: Some(t), column: c } if t == "users" && c == "age")
    );
}

// ── Predicate building ────────────────────────────────────────

#[test]
fn build_comparison() {
    let val = json!({"type": "comparison", "left": {"type": "column_ref", "column": "age"}, "op": "gt", "right": {"type": "literal", "value": 18, "data_type": "int"}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Comparison { op: Gt, .. }));
}

#[test]
fn build_and() {
    let val = json!({"type": "and", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}, "right": {"type": "comparison", "left": {"column": "b"}, "op": "gt", "right": {"value": 2}}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::And { .. }));
}

#[test]
fn build_or() {
    let val = json!({"type": "or", "left": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}, "right": {"type": "comparison", "left": {"column": "b"}, "op": "eq", "right": {"value": 2}}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Or { .. }));
}

#[test]
fn build_not() {
    let val = json!({"type": "not", "child": {"type": "comparison", "left": {"column": "a"}, "op": "eq", "right": {"value": 1}}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Not { .. }));
}

#[test]
fn build_between() {
    let val = json!({"type": "between", "expr": {"column": "age"}, "low": {"value": 18}, "high": {"value": 65}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Between { .. }));
}

#[test]
fn build_in_values() {
    let val = json!({"type": "in", "expr": {"column": "status"}, "target": [{"value": "active"}, {"value": "pending"}]});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(
        pred,
        Predicate::In {
            target: InTarget::Values(_),
            ..
        }
    ));
}

#[test]
fn build_like() {
    let val = json!({"type": "like", "expr": {"column": "name"}, "pattern": "%john%"});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Like { pattern, .. } if pattern == "%john%"));
}

#[test]
fn build_is_null() {
    let val = json!({"type": "is_null", "expr": {"column": "deleted_at"}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::IsNull { .. }));
}

#[test]
fn build_exists() {
    let val = json!({"type": "exists", "query": {"select": [{"type": "star"}], "from": {"table": "users"}}});
    let pred = build_predicate(&val).unwrap();
    assert!(matches!(pred, Predicate::Exists { .. }));
}

// ── Error cases ───────────────────────────────────────────────

#[test]
fn error_on_missing_type() {
    let val = json!({"unknown": "field"});
    let result = build_expression(&val);
    assert!(result.is_err());
}

#[test]
fn error_on_unknown_expression_type() {
    let val = json!({"type": "nonexistent"});
    let result = build_expression(&val);
    assert!(result.is_err());
}

#[test]
fn error_on_unknown_predicate_type() {
    let val = json!({"type": "nonexistent"});
    let result = build_predicate(&val);
    assert!(result.is_err());
}

#[test]
fn error_on_missing_op() {
    let val = json!({"type": "comparison", "left": {"column": "a"}, "right": {"value": 1}});
    let result = build_predicate(&val);
    assert!(result.is_err());
}

// ── Parser helpers ────────────────────────────────────────────

#[test]
fn parse_comparison_ops() {
    assert_eq!(parse_comparison_op("eq").unwrap(), Eq);
    assert_eq!(parse_comparison_op("ne").unwrap(), Neq);
    assert_eq!(parse_comparison_op("gt").unwrap(), Gt);
    assert_eq!(parse_comparison_op("gte").unwrap(), Gte);
    assert_eq!(parse_comparison_op("lt").unwrap(), Lt);
    assert_eq!(parse_comparison_op("lte").unwrap(), Lte);
    assert!(parse_comparison_op("unknown").is_err());
}

#[test]
fn parse_binary_ops() {
    use BinaryOperator::*;
    assert_eq!(parse_binary_op("add").unwrap(), Add);
    assert_eq!(parse_binary_op("sub").unwrap(), Sub);
    assert_eq!(parse_binary_op("mul").unwrap(), Mul);
    assert_eq!(parse_binary_op("div").unwrap(), Div);
    assert_eq!(parse_binary_op("mod").unwrap(), Mod);
    assert!(parse_binary_op("unknown").is_err());
}

#[test]
fn parse_data_types() {
    assert_eq!(parse_data_type("int").unwrap(), Int);
    assert_eq!(parse_data_type("string").unwrap(), DataType::String);
    assert_eq!(parse_data_type("float").unwrap(), Float);
    assert_eq!(parse_data_type("boolean").unwrap(), Boolean);
    assert_eq!(parse_data_type("timestamp").unwrap(), Timestamp);
    assert_eq!(parse_data_type("null").unwrap(), Null);
    assert!(parse_data_type("unknown").is_err());
}

#[test]
fn parse_join_types() {
    assert_eq!(parse_join_type("inner").unwrap(), Inner);
    assert_eq!(parse_join_type("left").unwrap(), Left);
    assert_eq!(parse_join_type("right").unwrap(), Right);
    assert_eq!(parse_join_type("full").unwrap(), Full);
    assert_eq!(parse_join_type("cross").unwrap(), Cross);
    assert!(parse_join_type("unknown").is_err());
}

#[test]
fn parse_new_data_types() {
    assert_eq!(
        parse_data_type("decimal").unwrap(),
        vlorql_core::schema::DataType::Decimal
    );
    assert_eq!(
        parse_data_type("array").unwrap(),
        vlorql_core::schema::DataType::Array
    );
    assert_eq!(
        parse_data_type("jsonb").unwrap(),
        vlorql_core::schema::DataType::Jsonb
    );
    assert_eq!(
        parse_data_type("blob").unwrap(),
        vlorql_core::schema::DataType::Blob
    );
    assert_eq!(
        parse_data_type("date").unwrap(),
        vlorql_core::schema::DataType::Date
    );
}
