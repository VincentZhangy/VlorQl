//! Aggregating schema, policy, operand, and dialect validation.

pub mod dialect;
pub mod operand;
pub mod pipeline;
mod schema;

pub use dialect::{BoundDialectValidator, DialectValidator};
pub use operand::OperandValidator;
pub use pipeline::{OptimizedPlan, ValidatedPlan, ValidationPipeline};
pub use schema::validate_schema;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{SchemaErrorKind, ValidationErrorKind, VlorQLError};
    use crate::policy::{PolicyConfig, PolicyEngine, TablePolicy};
    use crate::schema::{
        BinaryOperator, ColumnSchema, CommonTableExpression, ComparisonOperator, DataType,
        DialectProfile, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
        QueryPlan, SchemaMetadata, SchemaSnapshot, TableSchema,
    };
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

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
                    column("active", DataType::Boolean),
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            SchemaMetadata::default(),
        ))
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
        }
    }

    fn literal(value: serde_json::Value, data_type: DataType) -> Expression {
        Expression::Literal { value, data_type }
    }

    #[test]
    fn schema_validation_collects_missing_table_and_column_errors() {
        let mut plan = base_plan();
        plan.select.push(Projection::Column {
            table: Some("users".to_owned()),
            column: "missing_column".to_owned(),
            alias: None,
        });
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "missing_table".to_owned(),
                alias: None,
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: literal(json!(1), DataType::Int),
            },
        }]);

        let errors = validate_schema(&plan, &schema())
            .expect_err("invalid schema references should be collected");
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Schema {
                kind: SchemaErrorKind::TableNotFound { table },
                ..
            } if table == "missing_table"
        )));
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Schema {
                kind: SchemaErrorKind::ColumnNotFound { column, .. },
                ..
            } if column == "missing_column"
        )));
    }

    #[test]
    fn operand_validator_collects_multiple_type_mismatches() {
        let mut plan = base_plan();
        plan.select.push(Projection::Expr {
            expression: Expression::BinaryOp {
                left: Box::new(literal(json!("one"), DataType::String)),
                op: BinaryOperator::Add,
                right: Box::new(literal(json!(1), DataType::Int)),
            },
            alias: Some("invalid_sum".to_owned()),
        });
        plan.r#where = Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: literal(json!(42), DataType::Int),
        });

        let errors = OperandValidator::validate(&plan, &schema())
            .expect_err("incompatible operand types should fail validation");
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|error| matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::TypeMismatch { .. },
                ..
            }
        )));
    }

    #[test]
    fn dialect_validator_collects_cte_join_and_function_errors() {
        let mut plan = base_plan();
        plan.select.push(Projection::Expr {
            expression: Expression::FunctionCall {
                name: "danger".to_owned(),
                args: Vec::new(),
                distinct: true,
            },
            alias: None,
        });
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("other".to_owned()),
            },
            on: Predicate::IsNull {
                expr: Expression::ColumnRef {
                    table: Some("other".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]);
        plan.ctes = Some(vec![CommonTableExpression {
            name: "user_ids".to_owned(),
            query: Box::new(base_plan()),, recursive: false
        }]);
        let profile = DialectProfile::builder()
            .supports_cte(false)
            .max_joins(0usize)
            .allowed_functions(vec!["count".to_owned()])
            .allow_distinct(false)
            .build()
            .expect("dialect profile should build");

        let errors = DialectValidator::bind(&profile)
            .validate(&plan)
            .expect_err("all dialect violations should be collected");
        let codes: HashSet<_> = errors.iter().map(VlorQLError::error_code).collect();
        assert!(codes.contains("V005"));
        assert!(codes.contains("V007"));
        assert!(codes.contains("V008"));
        assert!(errors.len() >= 4);
    }

    #[test]
    fn pipeline_returns_all_stage_errors_together() {
        let mut plan = base_plan();
        plan.select.push(Projection::Column {
            table: Some("users".to_owned()),
            column: "missing_column".to_owned(),
            alias: None,
        });
        plan.select.push(Projection::Expr {
            expression: Expression::FunctionCall {
                name: "danger".to_owned(),
                args: Vec::new(),
                distinct: false,
            },
            alias: None,
        });
        plan.r#where = Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: literal(json!(99), DataType::Int),
        });
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("other".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("other".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]);
        plan.ctes = Some(vec![CommonTableExpression {
            name: "user_ids".to_owned(),
            query: Box::new(base_plan()),, recursive: false
        }]);

        let policy = PolicyEngine::new(PolicyConfig {
            table_policies: HashMap::from([(
                "users".to_owned(),
                TablePolicy {
                    allowed: false,
                    ..TablePolicy::default()
                },
            )]),
            ..PolicyConfig::default()
        });
        let dialect = DialectProfile::builder()
            .supports_cte(false)
            .max_joins(0usize)
            .allowed_functions(vec!["count".to_owned()])
            .build()
            .expect("dialect profile should build");
        let pipeline = ValidationPipeline::new(schema(), dialect, policy);

        let errors = pipeline
            .validate(&plan)
            .expect_err("the pipeline should aggregate every stage");
        let codes: HashSet<_> = errors
            .as_slice()
            .iter()
            .map(VlorQLError::error_code)
            .collect();
        assert!(codes.contains("S002"));
        assert!(codes.contains("P001"));
        assert!(codes.contains("V005"));
        assert!(codes.contains("V006"));
        assert!(codes.contains("V007"));
        assert!(codes.contains("V008"));
        assert!(errors.len() >= 6);
    }

    #[test]
    fn pipeline_wraps_a_valid_plan() {
        let plan = base_plan();
        let pipeline = ValidationPipeline::new(
            schema(),
            DialectProfile::default(),
            PolicyEngine::new(PolicyConfig::default()),
        );

        let validated = pipeline
            .validate(&plan)
            .expect("valid plan should pass every stage");
        assert_eq!(validated.as_plan(), &plan);
        assert_eq!(*validated.into_inner(), plan);
    }

    #[test]
    fn operand_validator_reports_string_versus_int_comparison() {
        // A common attack is to embed a string in a numeric predicate;
        // the operand validator must report the type mismatch.
        let snapshot = Arc::new(SchemaSnapshot::new(
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
        ));
        let mut plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: Some(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: serde_json::Value::String("not-a-number".to_owned()),
                    data_type: DataType::Int,
                },
            }),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
        };
        let errors = OperandValidator::validate(&plan, &snapshot)
            .expect_err("string literal declared as Int must fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::TypeMismatch { .. },
                ..
            }
        )));

        // A LIKE expression must operate on a string-typed column.
        plan.r#where = Some(Predicate::Like {
            expr: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            pattern: "%oops%".to_owned(),
        });
        let errors = OperandValidator::validate(&plan, &snapshot)
            .expect_err("LIKE on an int column must fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::TypeMismatch { .. },
                ..
            }
        )));
    }

    #[test]
    fn dialect_validator_reports_disabled_cte_with_collector() {
        let mut plan = base_plan();
        plan.ctes = Some(vec![CommonTableExpression {
            name: "active_users".to_owned(),
            query: Box::new(base_plan()),, recursive: false
        }]);
        let profile = DialectProfile::builder()
            .supports_cte(false)
            .build()
            .expect("dialect should build");
        let errors = DialectValidator::bind(&profile)
            .validate(&plan)
            .expect_err("CTE must be rejected");
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::DialectFeatureDisabled { feature },
                ..
            } if feature == "common_table_expressions"
        )));
    }

    #[test]
    fn dialect_validator_reports_too_many_joins() {
        let mut plan = base_plan();
        plan.joins = Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "users".to_owned(),
                    alias: Some("u2".to_owned()),
                },
                on: Predicate::IsNull {
                    expr: Expression::ColumnRef {
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
                on: Predicate::IsNull {
                    expr: Expression::ColumnRef {
                        table: Some("u3".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            },
        ]);
        let profile = DialectProfile::builder()
            .max_joins(1usize)
            .build()
            .expect("dialect should build");
        let errors = DialectValidator::bind(&profile)
            .validate(&plan)
            .expect_err("two joins with cap=1 should fail");
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Validation {
                kind: ValidationErrorKind::TooManyJoins { actual: 2, max: 1 },
                ..
            }
        )));
    }

    #[test]
    fn schema_validator_collects_unknown_table_in_qualified_column_reference() {
        // A column reference with a qualifier that is not in scope
        // must be surfaced as a schema error.
        let snapshot = schema();
        let plan = QueryPlan {
            select: vec![Projection::Column {
                table: Some("missing".to_owned()),
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
        let errors = validate_schema(&plan, &snapshot)
            .expect_err("qualifier that resolves to no source must be reported");
        assert!(errors.iter().any(|error| matches!(
            error,
            VlorQLError::Schema {
                kind: SchemaErrorKind::TableNotFound { table },
                ..
            } if table == "missing"
        )));
    }
}
