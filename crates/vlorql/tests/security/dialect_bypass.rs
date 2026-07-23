//! Dialect-feature bypass attempts.
//!
//! These tests verify that [`DialectProfile`] constraints (CTE, JOIN
//! count, OFFSET, allowed functions, allowed join types) cannot be
//! sidestepped by a hostile LLM.
//!
//! Every plan is validated through [`DialectValidator::validate`]
//! and is expected to surface a [`VlorQLError::Validation`] error of
//! the corresponding kind.

use std::sync::Arc;

use vlorql_core::errors::{ValidationErrorKind, VlorQLError};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, DialectProfile, Expression, FromClause,
    IdentifierQuoting, JoinClause, JoinType, Predicate, Projection, QueryPlan, SchemaMetadata,
    SqlDialect, TableSchema,
};
use vlorql_core::validate::dialect::DialectValidator;

#[allow(dead_code)]
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

fn base_plan() -> QueryPlan {
    QueryPlan {
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
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,    }
}

fn restricted_profile() -> DialectProfile {
    DialectProfile {
        dialect: SqlDialect::Sqlite,
        quote_style: IdentifierQuoting::DoubleQuote,
        supports_cte: false,
        supports_window_functions: false,
        supports_json_operations: false,
        max_joins: Some(2),
        allowed_join_types: vec![JoinType::Inner],
        allowed_functions: vec!["count".to_owned()],
        denied_functions: vec!["load_extension".to_owned()],
        max_group_by_columns: Some(2),
        allow_distinct: false,
        supports_offset: false,
        supports_fetch: false,
    }
}

fn assert_validation_error(
    errors: &[VlorQLError],
    predicate: impl Fn(&ValidationErrorKind) -> bool,
) {
    assert!(
        errors.iter().any(|error| matches!(
            error,
            VlorQLError::Validation { kind, .. } if predicate(kind)
        )),
        "no matching validation error in: {errors:#?}"
    );
}

// ---------------------------------------------------------------------
// 1. CTE
// ---------------------------------------------------------------------

#[test]
fn cte_is_rejected_when_profile_disables_it() {
    let mut plan = base_plan();
    plan.ctes = Some(vec![vlorql_core::schema::CommonTableExpression {
        name: "active_users".to_owned(),
        query: Box::new(base_plan()),, recursive: false
    }]);
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("CTE should be rejected");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::DialectFeatureDisabled { feature } if feature == "common_table_expressions"),
    );
}

#[test]
fn cte_in_nested_query_is_rejected_recursively() {
    // The hostile plan hides a CTE inside another CTE's body. The
    // dialect validator must recurse into every CTE so the inner one
    // is also caught by the `supports_cte = false` profile.
    let inner_cte = vlorql_core::schema::CommonTableExpression {
        name: "staging".to_owned(),
        query: Box::new(QueryPlan {
            ctes: Some(vec![vlorql_core::schema::CommonTableExpression {
                name: "inner_staging".to_owned(),
                query: Box::new(base_plan()),, recursive: false
            }]),
            ..base_plan()
        }),
    };
    let mut outer = base_plan();
    outer.ctes = Some(vec![inner_cte]);
    let errors = DialectValidator::validate(&outer, &restricted_profile())
        .expect_err("nested CTE must be rejected by recursive validation");
    // Both the outer and the inner CTE should be flagged.
    let feature_errors: Vec<_> = errors
        .iter()
        .filter_map(|error| match error {
            VlorQLError::Validation {
                kind: ValidationErrorKind::DialectFeatureDisabled { feature },
                ..
            } => Some(feature.clone()),
            _ => None,
        })
        .collect();
    assert!(
        feature_errors
            .iter()
            .all(|feature| feature == "common_table_expressions"),
        "expected only CTE feature errors, got {feature_errors:?}"
    );
    assert!(
        feature_errors.len() >= 2,
        "expected at least two CTE errors (outer + nested), got {feature_errors:?}"
    );
}

// ---------------------------------------------------------------------
// 2. JOIN count
// ---------------------------------------------------------------------

#[test]
fn too_many_joins_is_rejected() {
    let mut plan = base_plan();
    plan.joins = Some(vec![
        JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u2".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u2".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
        JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u3".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u3".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
        JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u4".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u4".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
    ]);
    // restricted_profile caps at 2 joins.
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("three joins should be rejected");
    assert_validation_error(&errors, |kind| {
        matches!(
            kind,
            ValidationErrorKind::TooManyJoins { actual: 3, max: 2 }
        )
    });
}

#[test]
fn disallowed_join_type_is_rejected() {
    let mut plan = base_plan();
    plan.joins = Some(vec![JoinClause {
        join_type: JoinType::Right,
        right_table: FromClause {
            table: "users".to_owned(),
            alias: Some("u2".to_owned()),
        },
        on: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::ColumnRef {
                table: Some("u2".to_owned()),
                column: "id".to_owned(),
            },
        },
    }]);
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("RIGHT JOIN must be rejected");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::DialectFeatureDisabled { feature } if feature.contains("right")),
    );
}

// ---------------------------------------------------------------------
// 3. OFFSET
// ---------------------------------------------------------------------

#[test]
fn offset_is_rejected_when_profile_disables_it() {
    let mut plan = base_plan();
    plan.offset = Some(10);
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("offset must be rejected when supports_offset = false");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::DialectFeatureDisabled { feature } if feature == "offset"),
    );
}

#[test]
fn offset_inside_a_cte_is_rejected() {
    // The hostile plan hides the offset inside a CTE. Dialect features
    // must be enforced recursively so the offset is still flagged.
    let mut cte_inner = base_plan();
    cte_inner.offset = Some(5);
    let mut plan = base_plan();
    plan.ctes = Some(vec![vlorql_core::schema::CommonTableExpression {
        name: "paged".to_owned(),
        query: Box::new(cte_inner),, recursive: false
    }]);
    // For this test we still need a CTE-enabled profile, otherwise the
    // CTE itself is rejected and we never reach the offset check.
    let mut profile = restricted_profile();
    profile.supports_cte = true;
    profile.supports_offset = false;
    let errors = DialectValidator::validate(&plan, &profile).expect_err("offset should fail");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::DialectFeatureDisabled { feature } if feature == "offset"),
    );
}

// ---------------------------------------------------------------------
// 4. Functions
// ---------------------------------------------------------------------

#[test]
fn function_not_in_allowlist_is_rejected() {
    let mut plan = base_plan();
    plan.select = vec![Projection::Expr {
        expression: Expression::FunctionCall {
            name: "sum".to_owned(),
            args: vec![Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            }],
            distinct: false,
        },
        alias: None,
    }];
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("`sum` is not in the allowlist and must be rejected");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::InvalidFunction { function, .. } if function == "sum"),
    );
}

#[test]
fn explicitly_denied_function_is_rejected() {
    let mut plan = base_plan();
    plan.select = vec![Projection::Expr {
        expression: Expression::FunctionCall {
            name: "load_extension".to_owned(),
            args: vec![Expression::Literal {
                value: serde_json::Value::String("/tmp/evil.so".to_owned()),
                data_type: DataType::String,
            }],
            distinct: false,
        },
        alias: None,
    }];
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("load_extension must always be rejected");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::InvalidFunction { function, .. } if function == "load_extension"),
    );
}

#[test]
fn distinct_keyword_is_rejected_when_disallowed() {
    let mut plan = base_plan();
    plan.select = vec![Projection::Expr {
        expression: Expression::FunctionCall {
            name: "count".to_owned(),
            args: vec![Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            }],
            distinct: true,
        },
        alias: None,
    }];
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("DISTINCT must be rejected when allow_distinct = false");
    assert_validation_error(
        &errors,
        |kind| matches!(kind, ValidationErrorKind::DialectFeatureDisabled { feature } if feature == "distinct"),
    );
}

// ---------------------------------------------------------------------
// 5. GROUP BY limit
// ---------------------------------------------------------------------

#[test]
fn too_many_group_by_columns_is_rejected() {
    let mut plan = base_plan();
    plan.group_by = Some(vec![
        Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
        Expression::ColumnRef {
            table: Some("users".to_owned()),
            column: "id".to_owned(),
        },
    ]);
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("3 GROUP BY columns should exceed the cap of 2");
    assert_validation_error(&errors, |kind| {
        matches!(kind, ValidationErrorKind::AggregationMismatch { .. })
    });
}

// ---------------------------------------------------------------------
// 6. Combined: a plan that violates several features at once collects
//    every error rather than failing fast on the first one.
// ---------------------------------------------------------------------

#[test]
fn multiple_dialect_violations_are_collected_together() {
    let mut plan = base_plan();
    plan.ctes = Some(vec![vlorql_core::schema::CommonTableExpression {
        name: "staging".to_owned(),
        query: Box::new(base_plan()),, recursive: false
    }]);
    plan.joins = Some(vec![
        JoinClause {
            join_type: JoinType::Right,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u2".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u2".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
        JoinClause {
            join_type: JoinType::Right,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u3".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u3".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
        JoinClause {
            join_type: JoinType::Right,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u4".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("u4".to_owned()),
                    column: "id".to_owned(),
                },
            },
        },
    ]);
    plan.offset = Some(7);
    plan.select = vec![Projection::Expr {
        expression: Expression::FunctionCall {
            name: "load_extension".to_owned(),
            args: vec![Expression::Literal {
                value: serde_json::Value::String("/tmp/evil.so".to_owned()),
                data_type: DataType::String,
            }],
            distinct: true,
        },
        alias: None,
    }];
    let errors = DialectValidator::validate(&plan, &restricted_profile())
        .expect_err("every violation should be reported");
    let codes: std::collections::HashSet<_> = errors.iter().map(VlorQLError::error_code).collect();
    assert!(codes.contains("V005"), "missing V005: {codes:?}");
    assert!(codes.contains("V007"), "missing V007: {codes:?}");
    assert!(codes.contains("V008"), "missing V008: {codes:?}");
}

// ---------------------------------------------------------------------
// 7. Sanity: a fully valid plan passes
// ---------------------------------------------------------------------

#[test]
fn plan_that_respects_every_constraint_passes_validation() {
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
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,    };
    DialectValidator::validate(&plan, &restricted_profile())
        .expect("a trivial plan should respect the strict profile");
}

// Keep the schema import non-`#[allow(dead_code)]` by referencing it in
// a no-op assertion. (The `TableSchema` helper above is only used in
// `base_plan`; the import is needed for `CommonTableExpression`.)
#[allow(dead_code)]
fn _force_schema_import() {
    let _ = Arc::new(vlorql_core::schema::SchemaSnapshot::new(
        Vec::<TableSchema>::new(),
        SchemaMetadata::default(),
    ));
}
