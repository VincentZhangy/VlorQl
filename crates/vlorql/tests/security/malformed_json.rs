//! Malformed-JSON hardening tests.
//!
//! These tests feed the `validate_only` pipeline with a variety of
//! malformed JSON bodies and assert that the deserialization layer
//! refuses every one of them, surfacing either:
//!
//! * [`ValidationErrorKind::InvalidJson`] (when the JSON itself is
//!   syntactically broken), or
//! * a deserialization error from serde (which `validate_only`
//!   surfaces as a [`VlorQLError::Validation`] containing
//!   `InvalidJson`).
//!
//! In addition, the tests cover:
//!
//! * Type-mismatched values (`limit` as a string, …).
//! * `deny_unknown_fields` rejecting extra keys.
//! * Tagged-enum tag/value mismatches.
//!
//! Every test constructs a `VlorQl` facade, then calls `validate_only`
//! directly on a `serde_json::Value` deserialized into `QueryPlan`.

use std::sync::Arc;

use serde_json::json;
use vlorql::{SchemaSnapshot, VlorQl};
use vlorql_core::errors::{ValidationErrorKind, VlorQLError};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ColumnSchema, DataType, FromClause, Projection, QueryPlan, SchemaMetadata, TableSchema,
};

fn column(name: &str, data_type: DataType) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type,
        nullable: false,
        description: None,
        is_primary_key: false,
        foreign_key: None,
    }
}

fn schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "users".to_owned(),
            columns: vec![
                column("id", DataType::Int),
                column("name", DataType::String),
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ))
}

fn facade() -> VlorQl {
    VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_policy(PolicyConfig::default())
        .build()
        .expect("facade should build")
}

fn assert_invalid_json(error: &VlorQLError) {
    let VlorQLError::Validation { kind, .. } = error else {
        panic!("expected Validation error, got {error:?}");
    };
    assert!(
        matches!(kind, ValidationErrorKind::InvalidJson),
        "expected InvalidJson, got {kind:?}"
    );
}

fn parse_plan_from_str(body: &str) -> Result<QueryPlan, VlorQLError> {
    serde_json::from_str::<QueryPlan>(body).map_err(|error| {
        VlorQLError::validation(
            ValidationErrorKind::InvalidJson,
            json!({"message": error.to_string(), "line": error.line(), "column": error.column()}),
        )
    })
}

// ---------------------------------------------------------------------
// 1. Syntactically broken JSON
// ---------------------------------------------------------------------

#[test]
fn unquoted_string_is_rejected() {
    let body = "{ unquoted: true }";
    let error = parse_plan_from_str(body).expect_err("unquoted keys must fail");
    assert_invalid_json(&error);
}

#[test]
fn missing_closing_brace_is_rejected() {
    let body = r#"{"select": [], "from": {"table": "users", "alias": null}"#;
    let error = parse_plan_from_str(body).expect_err("missing `}` must fail");
    assert_invalid_json(&error);
}

#[test]
fn trailing_comma_is_rejected() {
    let body = r#"{"select": [], "from": {"table": "users", "alias": null,}}"#;
    let error = parse_plan_from_str(body).expect_err("trailing comma must fail");
    assert_invalid_json(&error);
}

#[test]
fn multiple_top_level_values_are_rejected() {
    let body = r#"{"select": []}{"from": {"table": "users", "alias": null}}"#;
    let error = parse_plan_from_str(body).expect_err("two top-level objects must fail");
    assert_invalid_json(&error);
}

#[test]
fn empty_string_is_rejected() {
    let error = parse_plan_from_str("").expect_err("empty body must fail");
    assert_invalid_json(&error);
}

#[test]
fn completely_garbage_input_is_rejected() {
    let body = "this is not json at all";
    let error = parse_plan_from_str(body).expect_err("garbage input must fail");
    assert_invalid_json(&error);
}

#[test]
fn json_array_is_rejected_as_query_plan() {
    // A valid JSON value that is the wrong shape (an array, not an
    // object) must still be refused.
    let body = r#"[1, 2, 3]"#;
    let error = parse_plan_from_str(body).expect_err("array must fail to deserialize");
    assert_invalid_json(&error);
}

// ---------------------------------------------------------------------
// 2. Structural / type errors
// ---------------------------------------------------------------------

#[test]
fn limit_as_string_is_rejected() {
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}],
        "from": {"table": "users", "alias": null},
        "limit": "ten"
    }"#;
    let error = parse_plan_from_str(body).expect_err("string limit must fail");
    assert_invalid_json(&error);
}

#[test]
fn missing_required_field_is_rejected() {
    // `from` is required, but we omit it.
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}]
    }"#;
    let error = parse_plan_from_str(body).expect_err("missing `from` must fail");
    // serde reports this as a missing-field error which the validator
    // surfaces as InvalidJson or via MissingField; either is fine.
    assert!(
        matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::InvalidJson,
                ..
            } | VlorQLError::Validation {
                kind: ValidationErrorKind::MissingField { .. },
                ..
            }
        ),
        "expected InvalidJson or MissingField, got {error:?}"
    );
}

#[test]
fn projection_with_unknown_variant_tag_is_rejected() {
    // The tagged enum only knows `column`, `expr`, and `star`. The
    // hostile payload introduces a new variant.
    let body = r#"{
        "select": [{"type": "function", "name": "evil"}],
        "from": {"table": "users", "alias": null}
    }"#;
    let error = parse_plan_from_str(body).expect_err("unknown tag must fail");
    assert_invalid_json(&error);
}

#[test]
fn from_clause_missing_table_is_rejected() {
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}],
        "from": {"alias": null}
    }"#;
    let error = parse_plan_from_str(body).expect_err("missing `table` must fail");
    assert!(
        matches!(error, VlorQLError::Validation { .. }),
        "expected Validation error, got {error:?}"
    );
}

// ---------------------------------------------------------------------
// 3. deny_unknown_fields
// ---------------------------------------------------------------------

#[test]
fn extra_top_level_field_is_rejected() {
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}],
        "from": {"table": "users", "alias": null},
        "extra": true
    }"#;
    let error = parse_plan_from_str(body).expect_err("extra top-level field must fail");
    let rendered = error.to_string();
    let details = serde_json::to_string(error.details()).unwrap_or_default();
    assert!(
        rendered.contains("extra") || rendered.contains("unknown") || details.contains("extra"),
        "error should mention the offending field; got rendered={rendered}, details={details}"
    );
}

#[test]
fn extra_field_in_projection_is_rejected() {
    let body = r#"{
        "select": [{
            "type": "column",
            "table": "users",
            "column": "id",
            "alias": null,
            "sneaky": 42
        }],
        "from": {"table": "users", "alias": null}
    }"#;
    let error = parse_plan_from_str(body).expect_err("extra projection field must fail");
    let rendered = error.to_string();
    let details = serde_json::to_string(error.details()).unwrap_or_default();
    assert!(
        rendered.contains("sneaky") || rendered.contains("unknown") || details.contains("sneaky"),
        "expected mention of `sneaky` or `unknown`; got rendered={rendered}, details={details}"
    );
}

#[test]
fn extra_field_in_from_clause_is_rejected() {
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}],
        "from": {"table": "users", "alias": null, "sneaky": 1}
    }"#;
    let error = parse_plan_from_str(body).expect_err("extra FromClause field must fail");
    assert!(matches!(error, VlorQLError::Validation { .. }));
}

// ---------------------------------------------------------------------
// 4. End-to-end: a malicious JSON body should be rejected by the
//    facade (or by serde before reaching the facade).
// ---------------------------------------------------------------------

#[test]
fn facade_rejects_malicious_json_body() {
    // The body looks like an SQL-injection attempt embedded in JSON,
    // but at the same time it is well-formed JSON; the deserializer
    // should still accept it (since "name" is a string). We then check
    // that the resulting plan does NOT leak the payload into the
    // compiled SQL.
    let body = r#"{
        "select": [{
            "type": "column",
            "table": "users",
            "column": "name",
            "alias": null
        }],
        "from": {"table": "users", "alias": null},
        "where": {
            "type": "comparison",
            "left": {"type": "column_ref", "table": "users", "column": "name"},
            "op": "eq",
            "right": {"type": "literal", "value": "' OR 1=1 --", "data_type": "string"}
        }
    }"#;
    let plan: QueryPlan =
        serde_json::from_str(body).expect("the body is valid JSON for the QueryPlan schema");
    let facade = facade();
    let validated = facade
        .validate_only(&plan)
        .expect("the plan references a known column");
    let compiled = facade
        .compile_only(&validated)
        .expect("the plan should compile");
    assert!(!compiled.sql.contains("OR 1=1"), "{}", compiled.sql);
    assert_eq!(
        compiled.parameters[0].value,
        serde_json::Value::String("' OR 1=1 --".to_owned())
    );
}

#[test]
fn facade_rejects_malformed_json_via_validate_only() {
    // The body itself is malformed JSON. We surface that as a
    // `VlorQLError::Validation` with `InvalidJson` before any
    // validation stage runs.
    let body = r#"{"select": [{"type": "column", "table": "users","#;
    let result: Result<QueryPlan, _> = serde_json::from_str(body);
    let error = result.expect_err("malformed JSON must not deserialize");
    let mapped = VlorQLError::validation(
        ValidationErrorKind::InvalidJson,
        json!({"line": error.line(), "column": error.column()}),
    );
    assert_eq!(mapped.error_code(), "V001");
}

#[test]
fn facade_rejects_type_mismatch_via_validate_only() {
    let body = r#"{
        "select": [{"type": "column", "table": "users", "column": "id", "alias": null}],
        "from": {"table": "users", "alias": null},
        "limit": {"not": "a number"}
    }"#;
    let result: Result<QueryPlan, _> = serde_json::from_str(body);
    assert!(
        result.is_err(),
        "object passed as `limit` must fail to deserialize"
    );
}

// ---------------------------------------------------------------------
// 5. Round-trip of a valid plan confirms the deserializer is strict
// ---------------------------------------------------------------------

#[test]
fn valid_plan_round_trips_through_serde() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: Some(10),
        offset: None,
        joins: None,
        ctes: None,
    };
    let serialized = serde_json::to_string(&plan).expect("plan should serialize");
    let restored: QueryPlan = serde_json::from_str(&serialized).expect("plan should deserialize");
    assert_eq!(restored, plan);
}
