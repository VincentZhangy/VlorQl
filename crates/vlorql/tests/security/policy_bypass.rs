//! Policy bypass attempts.
//!
//! These tests assume the role of a hostile LLM (or a malicious
//! human-in-the-loop operator) that tries to read or reference a
//! table/column that has been denied by the access policy. They
//! exercise:
//!
//! * Case-mismatched identifiers (`USERS` vs `users`).
//! * Trailing whitespace and Unicode look-alikes.
//! * Alias-based reference of a denied table.
//! * `JOIN`-time references to denied tables or columns.
//! * Globally denied columns referenced from any table.
//! * Mandatory row filters that try to be neutralized by the plan
//!   author.
//!
//! Every attempt must end in a [`VlorQLError::Policy`] (or, where the
//! schema check fires first, [`VlorQLError::Schema`]) being raised by
//! the validation pipeline. None of them should reach the SQL compiler.

use std::collections::HashMap;
use std::sync::Arc;

use vlorql::{SchemaSnapshot, VlorQl};
use vlorql_core::errors::{PolicyErrorKind, SchemaErrorKind, VlorQLError};
use vlorql_core::policy::{PolicyConfig, TablePolicy};
use vlorql_core::schema::{
    ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, JoinClause, JoinType,
    Predicate, Projection, QueryPlan, SchemaMetadata, TableSchema,
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
        vec![
            TableSchema {
                name: "users".to_owned(),
                columns: vec![
                    column("id", DataType::Int),
                    column("name", DataType::String),
                    column("password_hash", DataType::String),
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "secrets".to_owned(),
                columns: vec![
                    column("id", DataType::Int),
                    column("value", DataType::String),
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            },
        ],
        SchemaMetadata::default(),
    ))
}

fn facade(policy: PolicyConfig) -> VlorQl {
    VlorQl::builder()
        .with_schema(schema())
        .with_dialect_name("sqlite")
        .with_policy(policy)
        .build()
        .expect("facade should build")
}

fn policy_with_restricted_users() -> PolicyConfig {
    let mut table_policies = HashMap::new();
    table_policies.insert(
        "users".to_owned(),
        TablePolicy {
            allowed: true,
            allowed_columns: Some(vec!["id".to_owned(), "name".to_owned()]),
            denied_columns: vec!["password_hash".to_owned()],
            row_filter: Some(vlorql_core::policy::RowFilter {
                condition: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Gt,
                    right: Expression::Literal {
                        value: serde_json::json!(0_i64),
                        data_type: DataType::Int,
                    },
                },
                description: "tenant isolation".to_owned(),
            }),
        },
    );
    table_policies.insert(
        "secrets".to_owned(),
        TablePolicy {
            allowed: false,
            ..TablePolicy::default()
        },
    );
    PolicyConfig {
        table_policies,
        global_denied_columns: vec!["password_hash".to_owned()],
        ..PolicyConfig::default()
    }
}

// ---------------------------------------------------------------------
// 1. Denied table referenced by an alternate identifier
// ---------------------------------------------------------------------

#[test]
fn denied_table_cannot_be_referenced_via_case_insensitive_alias() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("USERS".to_owned()),
            column: "name".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "USERS".to_owned(),
            alias: Some("u".to_owned()),
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("uppercase `USERS` should be rejected");
    // The schema check fires first: `USERS` is not the same identifier
    // as `users` and so the table does not exist.
    assert!(
        errors.as_slice().iter().any(|error| matches!(
            error,
            VlorQLError::Schema {
                kind: SchemaErrorKind::TableNotFound { table },
                ..
            } if table == "USERS"
        )),
        "expected schema to reject `USERS`, got {:?}",
        errors.as_slice()
    );
}

#[test]
fn denied_table_cannot_be_referenced_via_trailing_whitespace() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users ".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users ".to_owned(),
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
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("`users ` (with trailing space) should be rejected");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Schema {
            kind: SchemaErrorKind::TableNotFound { table },
            ..
        } if table == "users "
    )));
}

#[test]
fn denied_table_cannot_be_referenced_via_unicode_lookalike() {
    // The Greek question mark ";" is a perfect look-alike of ASCII ";".
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("\u{037E}users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "\u{037E}users".to_owned(),
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
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("unicode-prefixed table name should be rejected");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Schema {
            kind: SchemaErrorKind::TableNotFound { .. },
            ..
        }
    )));
}

#[test]
fn denied_secrets_table_is_rejected_even_when_aliased() {
    // The plan author tries to hide the reference by aliasing the
    // denied table; the policy must still catch it.
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("s".to_owned()),
            column: "value".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "secrets".to_owned(),
            alias: Some("s".to_owned()),
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("the `secrets` table must be denied by policy");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { table },
            ..
        } if table == "secrets"
    )));
}

// ---------------------------------------------------------------------
// 2. Denied column in an allowed table
// ---------------------------------------------------------------------

#[test]
fn table_denied_column_is_rejected() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "password_hash".to_owned(),
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
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("password_hash must be denied");
    // The denylist in `TablePolicy::denied_columns` is the strongest
    // signal, so the resulting error must come from the policy engine.
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { table, column },
            ..
        } if table == "users" && column == "password_hash"
    )));
}

#[test]
fn globally_denied_column_is_rejected_from_any_table() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "password_hash".to_owned(),
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
    };
    let mut policy = policy_with_restricted_users();
    // Remove the per-table denylist so the only thing that catches the
    // attempt is the global one.
    policy
        .table_policies
        .get_mut("users")
        .expect("users policy should exist")
        .denied_columns
        .clear();
    let errors = facade(policy)
        .validate_only(&plan)
        .expect_err("globally denied column must still be denied");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { column, .. },
            ..
        } if column == "password_hash"
    )));
}

#[test]
fn column_outside_allowlist_is_rejected() {
    // The users policy has allowed_columns = [id, name]. Selecting any
    // other column on `users` should be denied.
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "password_hash".to_owned(),
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
    };
    let mut policy = policy_with_restricted_users();
    policy
        .table_policies
        .get_mut("users")
        .expect("users policy should exist")
        .denied_columns
        .clear();
    let errors = facade(policy)
        .validate_only(&plan)
        .expect_err("column not in allowlist should be denied");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { column, .. },
            ..
        } if column == "password_hash"
    )));
}

// ---------------------------------------------------------------------
// 3. JOIN-based bypass attempts
// ---------------------------------------------------------------------

#[test]
fn joining_against_a_denied_table_is_rejected() {
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
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
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "secrets".to_owned(),
                alias: Some("s".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("s".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("joining against a denied table should be rejected");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { table },
            ..
        } if table == "secrets"
    )));
}

#[test]
fn selecting_from_a_denied_table_via_join_alias_is_rejected() {
    // The projection uses the join's alias rather than the table name.
    // The policy engine must trace the alias back to the denied table.
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("s".to_owned()),
            column: "value".to_owned(),
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
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "secrets".to_owned(),
                alias: Some("s".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("s".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("aliased reference to a denied table should be rejected");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { table },
            ..
        } if table == "secrets"
    )));
}

#[test]
fn selecting_a_denied_column_via_join_alias_is_rejected() {
    // Even when the table itself is allowed, a column from a denied
    // join should be rejected by the per-table policy.
    let plan = QueryPlan {
        select: vec![Projection::Column {
            table: Some("u".to_owned()),
            column: "password_hash".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("u".to_owned()),
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "secrets".to_owned(),
                alias: Some("s".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("u".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("s".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("denied column via aliased reference should be rejected");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::ColumnDenied { column, .. },
            ..
        } if column == "password_hash"
    )));
}

// ---------------------------------------------------------------------
// 4. Globally denied column referenced from a JOIN
// ---------------------------------------------------------------------

#[test]
fn globally_denied_column_referenced_via_join_is_rejected() {
    // The `secrets` table is denied entirely, but even if a hostile
    // plan author tried to join it, the globally denied `value`
    // column should still be caught.
    let plan = QueryPlan {
        select: vec![Projection::Star {
            table: Some("users".to_owned()),
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
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "secrets".to_owned(),
                alias: Some("s".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("s".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        ctes: None,
    };
    let errors = facade(policy_with_restricted_users())
        .validate_only(&plan)
        .expect_err("joining the secrets table must fail");
    assert!(errors.as_slice().iter().any(|error| matches!(
        error,
        VlorQLError::Policy {
            kind: PolicyErrorKind::TableDenied { table },
            ..
        } if table == "secrets"
    )));
}

// ---------------------------------------------------------------------
// 5. Mandatory row filters are non-bypassable
// ---------------------------------------------------------------------

#[test]
fn row_filter_is_applied_even_when_plan_author_omits_a_where_clause() {
    use vlorql_core::policy::PolicyEngine;
    let policy_engine = PolicyEngine::new(policy_with_restricted_users());
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
    };
    let predicate = policy_engine
        .apply_row_filters(&plan)
        .expect("a row filter must be derived from the policy");
    // The filter must enforce the same positive comparison that the
    // policy declared, so the plan cannot simply omit WHERE.
    let rendered = serde_json::to_string(&predicate).expect("predicate should serialize");
    assert!(rendered.contains("users"));
    assert!(rendered.contains("id"));
    assert!(rendered.contains("0"));
}

#[test]
fn row_filter_combines_multiple_conditions_with_and() {
    use vlorql_core::policy::{PolicyEngine, RowFilter};
    let mut policy = policy_with_restricted_users();
    policy.row_filters.push(RowFilter {
        condition: Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Lt,
            right: Expression::Literal {
                value: serde_json::json!(1_000_000_i64),
                data_type: DataType::Int,
            },
        },
        description: "global id bound".to_owned(),
    });
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
    };
    let predicate = PolicyEngine::new(policy)
        .apply_row_filters(&plan)
        .expect("two filters should combine");
    let value = serde_json::to_value(&predicate).expect("predicate should serialize");
    assert_eq!(value["type"], "and", "two filters must combine via AND");
    assert_eq!(value["left"]["type"], "comparison");
    assert_eq!(value["right"]["type"], "comparison");
}
