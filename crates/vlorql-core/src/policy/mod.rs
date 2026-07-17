//! Access-control policy configuration and evaluation.

pub mod config;
pub mod engine;

pub use config::{PolicyConfig, RowFilter, TablePolicy};
pub use engine::PolicyEngine;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{PolicyErrorKind, SchemaErrorKind, VlorQLError};
    use crate::schema::{
        ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, JoinClause, JoinType,
        Predicate, Projection, QueryPlan, SchemaMetadata, SchemaSnapshot, TableSchema,
    };
    use std::collections::HashMap;

    fn column(name: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_owned(),
            data_type: DataType::String,
            nullable: false,
            description: None,
            is_primary_key: false,
            foreign_key: None,
        }
    }

    fn schema() -> SchemaSnapshot {
        SchemaSnapshot::new(
            vec![
                TableSchema {
                    name: "users".to_owned(),
                    columns: vec![
                        column("id"),
                        column("email"),
                        column("password_hash"),
                        column("tenant_id"),
                        column("active"),
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
                TableSchema {
                    name: "accounts".to_owned(),
                    columns: vec![column("id"), column("owner_id")],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
            ],
            SchemaMetadata::default(),
        )
    }

    fn plan_with_columns(columns: &[&str]) -> QueryPlan {
        QueryPlan {
            select: columns
                .iter()
                .map(|column| Projection::Column {
                    table: Some("users".to_owned()),
                    column: (*column).to_owned(),
                    alias: None,
                })
                .collect(),
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
        }
    }

    fn column_ref(table: &str, column: &str) -> Expression {
        Expression::ColumnRef {
            table: Some(table.to_owned()),
            column: column.to_owned(),
        }
    }

    fn equality_filter(table: &str, column: &str, value: serde_json::Value) -> RowFilter {
        RowFilter {
            condition: Predicate::Comparison {
                left: column_ref(table, column),
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value,
                    data_type: DataType::String,
                },
            },
            description: format!("Restrict {table}.{column}"),
        }
    }

    #[test]
    fn validate_allows_unrestricted_table_and_columns() {
        let engine = PolicyEngine::new(PolicyConfig::default());
        let plan = plan_with_columns(&["id", "email"]);

        assert!(engine.validate(&plan, &schema()).is_ok());
        assert!(engine.is_column_allowed("users", "email"));
    }

    #[test]
    fn validate_rejects_denied_table() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    allowed: false,
                    ..TablePolicy::default()
                },
            )]),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let errors = engine
            .validate(&plan_with_columns(&["id"]), &schema())
            .expect_err("denied table should fail validation");

        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            VlorQLError::Policy {
                kind: PolicyErrorKind::TableDenied { table },
                ..
            } if table == "users"
        ));
    }

    #[test]
    fn validate_collects_allowlist_and_denylist_violations() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    allowed: true,
                    allowed_columns: Some(vec!["id".to_owned(), "password_hash".to_owned()]),
                    denied_columns: vec!["password_hash".to_owned()],
                    row_filter: None,
                },
            )]),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let errors = engine
            .validate(
                &plan_with_columns(&["id", "email", "password_hash"]),
                &schema(),
            )
            .expect_err("two columns should violate policy");

        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|error| matches!(
            error,
            VlorQLError::Policy {
                kind: PolicyErrorKind::ColumnDenied { table, .. },
                ..
            } if table == "users"
        )));
    }

    #[test]
    fn validate_enforces_global_denied_columns_in_nested_expressions() {
        let engine = PolicyEngine::new(PolicyConfig {
            global_denied_columns: vec!["password_hash".to_owned()],
            ..PolicyConfig::default()
        });
        let mut plan = plan_with_columns(&["id"]);
        plan.r#where = Some(Predicate::IsNull {
            expr: column_ref("users", "password_hash"),
        });

        let errors = engine
            .validate(&plan, &schema())
            .expect_err("globally denied column should fail validation");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].error_code(), "P002");
        assert_eq!(errors[0].details()["reason"], "global_denied_column");
    }

    #[test]
    fn validate_reports_missing_schema_table() {
        let mut plan = plan_with_columns(&["id"]);
        plan.from.table = "missing".to_owned();
        if let Projection::Column { table, .. } = &mut plan.select[0] {
            *table = Some("missing".to_owned());
        }

        let errors = PolicyEngine::new(PolicyConfig::default())
            .validate(&plan, &schema())
            .expect_err("unknown table should fail validation");
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            VlorQLError::Schema {
                kind: SchemaErrorKind::TableNotFound { table },
                ..
            } if table == "missing"
        ));
    }

    #[test]
    fn validate_checks_joined_tables_and_alias_qualified_columns() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "accounts".to_owned(),
                TablePolicy {
                    allowed: false,
                    ..TablePolicy::default()
                },
            )]),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let mut plan = plan_with_columns(&["id"]);
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "accounts".to_owned(),
                alias: Some("a".to_owned()),
            },
            on: Predicate::Comparison {
                left: column_ref("users", "id"),
                op: ComparisonOperator::Eq,
                right: column_ref("a", "owner_id"),
            },
        }]);

        let errors = engine
            .validate(&plan, &schema())
            .expect_err("denied joined table should fail validation");
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            VlorQLError::Policy {
                kind: PolicyErrorKind::TableDenied { table },
                ..
            } if table == "accounts"
        ));
    }

    #[test]
    fn validate_expands_star_for_column_policy_checks() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    denied_columns: vec!["password_hash".to_owned()],
                    ..TablePolicy::default()
                },
            )]),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let mut plan = plan_with_columns(&[]);
        plan.select = vec![Projection::Star {
            table: Some("users".to_owned()),
        }];

        let errors = engine
            .validate(&plan, &schema())
            .expect_err("star must not bypass denied columns");
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            VlorQLError::Policy {
                kind: PolicyErrorKind::ColumnDenied { column, .. },
                ..
            } if column == "password_hash"
        ));
    }

    #[test]
    fn apply_row_filters_combines_table_and_global_filters() {
        let table_filter = equality_filter("users", "tenant_id", serde_json::json!("tenant-1"));
        let global_filter = equality_filter("users", "active", serde_json::json!(true));
        let unrelated_filter = equality_filter("accounts", "owner_id", serde_json::json!("owner"));
        let engine = PolicyEngine::new(PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    row_filter: Some(table_filter.clone()),
                    ..TablePolicy::default()
                },
            )]),
            row_filters: vec![global_filter.clone(), unrelated_filter],
            ..PolicyConfig::default()
        });

        let combined = engine
            .apply_row_filters(&plan_with_columns(&["id"]))
            .expect("matching filters should be combined");
        assert_eq!(
            combined,
            Predicate::And {
                left: Box::new(table_filter.condition),
                right: Box::new(global_filter.condition),
            }
        );
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use crate::errors::{PolicyErrorKind, VlorQLError};
    use crate::schema::{
        ColumnSchema, ComparisonOperator, DataType, Expression, FromClause, JoinClause, JoinType,
        Predicate, Projection, QueryPlan, SchemaMetadata, SchemaSnapshot, TableSchema,
    };
    use std::collections::HashMap;

    fn select_user_columns(columns: &[&str]) -> QueryPlan {
        QueryPlan {
            select: columns
                .iter()
                .map(|column| Projection::Column {
                    table: Some("users".to_owned()),
                    column: (*column).to_owned(),
                    alias: None,
                })
                .collect(),
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
        }
    }

    #[test]
    fn is_column_allowed_distinguishes_allowlist_and_globally_denied() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    allowed: true,
                    allowed_columns: Some(vec!["id".to_owned()]),
                    denied_columns: vec![],
                    row_filter: None,
                },
            )]),
            global_denied_columns: vec!["secret".to_owned()],
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        assert!(engine.is_column_allowed("users", "id"));
        assert!(!engine.is_column_allowed("users", "email"));
        assert!(!engine.is_column_allowed("users", "secret"));
    }

    #[test]
    fn apply_row_filters_returns_none_for_unrelated_table() {
        let config = PolicyConfig {
            row_filters: vec![RowFilter {
                condition: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("other".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: serde_json::json!(1),
                        data_type: DataType::Int,
                    },
                },
                description: "other-table filter".to_owned(),
            }],
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let plan = select_user_columns(&["id"]);
        assert!(engine.apply_row_filters(&plan).is_none());
    }

    #[test]
    fn validate_collects_table_and_column_violations_in_one_pass() {
        // A hostile plan selects from a denied table and joins to
        // another table whose `allowed_columns` excludes the join
        // predicate column. Every violation must be reported, not
        // just the first one encountered.
        let snapshot = SchemaSnapshot::new(
            vec![
                TableSchema {
                    name: "users".to_owned(),
                    columns: vec![ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: true,
                        foreign_key: None,
                    }],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
                TableSchema {
                    name: "accounts".to_owned(),
                    columns: vec![
                        ColumnSchema {
                            name: "id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: true,
                            foreign_key: None,
                        },
                        ColumnSchema {
                            name: "owner_id".to_owned(),
                            data_type: DataType::Int,
                            nullable: false,
                            description: None,
                            is_primary_key: false,
                            foreign_key: None,
                        },
                    ],
                    description: None,
                    primary_key: Some(vec!["id".to_owned()]),
                },
            ],
            SchemaMetadata::default(),
        );
        let config = PolicyConfig {
            table_policies: HashMap::from([
                (
                    "users".to_owned(),
                    TablePolicy {
                        allowed: false,
                        ..TablePolicy::default()
                    },
                ),
                (
                    "accounts".to_owned(),
                    TablePolicy {
                        allowed: true,
                        allowed_columns: Some(vec!["id".to_owned()]),
                        denied_columns: vec!["owner_id".to_owned()],
                        row_filter: None,
                    },
                ),
            ]),
            global_denied_columns: vec!["password_hash".to_owned()],
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
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
            joins: Some(vec![JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "accounts".to_owned(),
                    alias: Some("a".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("a".to_owned()),
                        column: "owner_id".to_owned(),
                    },
                },
            }]),
            ctes: None,
        };
        let errors = engine
            .validate(&plan, &snapshot)
            .expect_err("denied table and denied column must both be reported");
        let codes: HashMap<&str, usize> = errors.iter().fold(HashMap::new(), |mut map, error| {
            *map.entry(error.error_code()).or_insert(0) += 1;
            map
        });
        assert!(
            codes.get("P001").copied().unwrap_or(0) >= 1,
            "codes: {codes:?}"
        );
        assert!(
            codes.get("P002").copied().unwrap_or(0) >= 1,
            "codes: {codes:?}"
        );
        let table_denied: Vec<_> = errors
            .iter()
            .filter_map(|error| match error {
                VlorQLError::Policy {
                    kind: PolicyErrorKind::TableDenied { table },
                    ..
                } => Some(table.clone()),
                _ => None,
            })
            .collect();
        assert!(table_denied.contains(&"users".to_owned()));
        let column_denied: Vec<_> = errors
            .iter()
            .filter_map(|error| match error {
                VlorQLError::Policy {
                    kind: PolicyErrorKind::ColumnDenied { table, column },
                    ..
                } => Some((table.clone(), column.clone())),
                _ => None,
            })
            .collect();
        assert!(column_denied.contains(&("accounts".to_owned(), "owner_id".to_owned())));
    }

    #[test]
    fn apply_row_filters_chains_three_conditions_left_associated() {
        // Three matching filters should produce a left-associated AND
        // tree: ((a AND b) AND c). The reduce-based implementation in
        // the engine processes the table-level filter first, then the
        // two global filters, so the outermost AND has the second
        // (and so on) filters on the right.
        fn make_filter(column: &str, value: serde_json::Value) -> RowFilter {
            RowFilter {
                condition: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: column.to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value,
                        data_type: DataType::String,
                    },
                },
                description: format!("filter on {column}"),
            }
        }
        let engine = PolicyEngine::new(PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    row_filter: Some(make_filter("tenant_id", serde_json::json!("tenant-1"))),
                    ..TablePolicy::default()
                },
            )]),
            row_filters: vec![
                make_filter("active", serde_json::json!(true)),
                make_filter("id", serde_json::json!(42)),
            ],
            ..PolicyConfig::default()
        });
        let plan = select_user_columns(&["id"]);
        let predicate = engine
            .apply_row_filters(&plan)
            .expect("three matching filters should combine");
        // Outer level: AND with one Comparison on the right and a
        // nested AND on the left.
        let Predicate::And { left, right } = predicate else {
            panic!("outer node should be AND, got {predicate:?}");
        };
        assert!(matches!(right.as_ref(), Predicate::Comparison { .. }));
        let Predicate::And {
            left: inner_left,
            right: inner_right,
        } = left.as_ref()
        else {
            panic!("left branch should be AND, got {left:?}");
        };
        assert!(matches!(inner_left.as_ref(), Predicate::Comparison { .. }));
        assert!(matches!(inner_right.as_ref(), Predicate::Comparison { .. }));
    }

    #[test]
    fn is_column_allowed_default_permits_unknown_tables() {
        let engine = PolicyEngine::new(PolicyConfig::default());
        // No per-table policy and no global denylist -> the column is
        // considered allowed even though the table is not in any
        // allowlist.
        assert!(engine.is_column_allowed("any_table", "any_column"));
    }

    #[test]
    fn get_policy_for_table_returns_none_for_unconfigured_tables() {
        let engine = PolicyEngine::new(PolicyConfig::default());
        assert!(engine.get_policy_for_table("users").is_none());
        assert!(engine.get_row_filter("users").is_none());
    }

    #[test]
    fn get_policy_for_table_returns_configured_policy() {
        let config = PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    allowed: true,
                    ..TablePolicy::default()
                },
            )]),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(config);
        let policy = engine
            .get_policy_for_table("users")
            .expect("users policy should exist");
        assert!(policy.allowed);
        assert_eq!(engine.get_row_filter("users"), None);
    }
}
