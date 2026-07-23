//! Safe, parameterized SQL compilation for supported dialects.

pub mod builder;
pub mod mysql;
pub mod postgres;
pub mod registry;
pub mod sqlite;
pub mod types;

pub use builder::QueryBuilder;
pub use mysql::MySQLCompiler;
pub use postgres::PostgresCompiler;
pub use registry::{CompilerRegistry, get_compiler};
pub use sqlite::SQLiteCompiler;
pub use types::{CompiledQuery, Parameter, SqlCompiler};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        BinaryOperator, CommonTableExpression, ComparisonOperator, DataType, Expression,
        FromClause, IdentifierQuoting, InTarget, JoinClause, JoinType, OrderByTerm, Predicate,
        Projection, QueryPlan, SetOperation, SetOperationClause, SqlDialect, WindowSpec,
    };
    use crate::validate::ValidatedPlan;
    use serde_json::json;
    use std::sync::Arc;

    fn column_ref(table: &str, column: &str) -> Expression {
        Expression::ColumnRef {
            table: Some(table.to_owned()),
            column: column.to_owned(),
        }
    }

    fn literal(value: serde_json::Value, data_type: DataType) -> Expression {
        Expression::Literal { value, data_type }
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
            set_operation: None,
        }
    }

    fn validated(plan: QueryPlan) -> ValidatedPlan {
        ValidatedPlan(Arc::new(plan))
    }

    #[test]
    fn postgres_compiles_simple_select_with_quoted_identifiers() {
        let mut plan = base_plan();
        plan.select.push(Projection::Column {
            table: Some("users".to_owned()),
            column: "name".to_owned(),
            alias: Some("display_name".to_owned()),
        });

        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("simple select should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\", \"users\".\"name\" AS \"display_name\" FROM \"users\""
        );
        assert!(compiled.parameters.is_empty());
        assert_eq!(compiled.dialect, SqlDialect::Postgres);
    }

    #[test]
    fn postgres_parameterizes_where_values_in_textual_order() {
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: column_ref("users", "id"),
                op: ComparisonOperator::Gt,
                right: literal(json!(10), DataType::Int),
            }),
            right: Box::new(Predicate::Like {
                expr: column_ref("users", "name"),
                pattern: "A%".to_owned(),
            }),
        });

        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("where clause should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\" FROM \"users\" WHERE (\"users\".\"id\" > $1) AND (\"users\".\"name\" LIKE $2)"
        );
        assert_eq!(
            compiled.parameters,
            vec![
                Parameter {
                    value: json!(10),
                    data_type: DataType::Int,
                },
                Parameter {
                    value: json!("A%"),
                    data_type: DataType::String,
                },
            ]
        );
    }

    #[test]
    fn builder_compiles_join_group_having_and_binary_expression() {
        let mut plan = base_plan();
        plan.from.alias = Some("u".to_owned());
        plan.select = vec![
            Projection::Column {
                table: Some("u".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "COUNT".to_owned(),
                    args: vec![column_ref("a", "id")],
                    distinct: false,
                },
                alias: Some("account_count".to_owned()),
            },
        ];
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "accounts".to_owned(),
                alias: Some("a".to_owned()),
            },
            on: Predicate::Comparison {
                left: column_ref("u", "id"),
                op: ComparisonOperator::Eq,
                right: column_ref("a", "owner_id"),
            },
        }]);
        plan.group_by = Some(vec![column_ref("u", "id")]);
        plan.having = Some(Predicate::Comparison {
            left: Expression::FunctionCall {
                name: "COUNT".to_owned(),
                args: vec![column_ref("a", "id")],
                distinct: false,
            },
            op: ComparisonOperator::Gt,
            right: literal(json!(1), DataType::Int),
        });
        plan.order_by = Some(vec![OrderByTerm {
            expr: Expression::BinaryOp {
                left: Box::new(column_ref("u", "id")),
                op: BinaryOperator::Add,
                right: Box::new(literal(json!(1), DataType::Int)),
            },
            descending: true,
        }]);

        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("complex clauses should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"u\".\"id\", COUNT(\"a\".\"id\") AS \"account_count\" FROM \"users\" AS \"u\" LEFT JOIN \"accounts\" AS \"a\" ON \"u\".\"id\" = \"a\".\"owner_id\" GROUP BY \"u\".\"id\" HAVING COUNT(\"a\".\"id\") > $1 ORDER BY (\"u\".\"id\" + $2) DESC"
        );
        assert_eq!(compiled.parameters.len(), 2);
        assert_eq!(compiled.parameters[0].value, json!(1));
        assert_eq!(compiled.parameters[1].value, json!(1));
    }

    #[test]
    fn dialects_use_expected_quoting_placeholders_and_pagination() {
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: column_ref("users", "active"),
            op: ComparisonOperator::Eq,
            right: literal(json!(true), DataType::Boolean),
        });
        plan.limit = Some(10);
        plan.offset = Some(20);
        let validated = validated(plan);

        let postgres = get_compiler(SqlDialect::Postgres)
            .compile(&validated)
            .expect("PostgreSQL should compile");
        let sqlite = CompilerRegistry::get(SqlDialect::Sqlite)
            .compile(&validated)
            .expect("SQLite should compile");
        let mysql = MySQLCompiler
            .compile(&validated)
            .expect("MySQL should compile");

        assert!(postgres.sql.contains("= $1 LIMIT $2 OFFSET $3"));
        assert!(
            sqlite
                .sql
                .contains("\"users\".\"active\" = ? LIMIT ? OFFSET ?")
        );
        assert!(mysql.sql.contains("`users`.`active` = ? LIMIT ?, ?"));
        assert_eq!(postgres.parameters, sqlite.parameters);
        assert_eq!(postgres.parameters.len(), 3);
        assert_ne!(sqlite.parameters, mysql.parameters);
    }

    #[test]
    fn cte_parameters_share_one_postgres_placeholder_sequence() {
        let mut cte_query = base_plan();
        cte_query.r#where = Some(Predicate::Comparison {
            left: column_ref("users", "active"),
            op: ComparisonOperator::Eq,
            right: literal(json!(true), DataType::Boolean),
        });
        let mut plan = base_plan();
        plan.from = FromClause {
            table: "active_users".to_owned(),
            alias: None,
        };
        plan.select = vec![Projection::Column {
            table: Some("active_users".to_owned()),
            column: "id".to_owned(),
            alias: None,
        }];
        plan.r#where = Some(Predicate::Comparison {
            left: column_ref("active_users", "id"),
            op: ComparisonOperator::Gt,
            right: literal(json!(100), DataType::Int),
        });
        plan.ctes = Some(vec![CommonTableExpression {
            name: "active_users".to_owned(),
            query: Box::new(cte_query), recursive: false
        }]);

        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("CTE should compile recursively");
        assert_eq!(
            compiled.sql,
            "WITH \"active_users\" AS (SELECT \"users\".\"id\" FROM \"users\" WHERE \"users\".\"active\" = $1) SELECT \"active_users\".\"id\" FROM \"active_users\" WHERE \"active_users\".\"id\" > $2"
        );
        assert_eq!(compiled.parameters.len(), 2);
        assert_eq!(compiled.parameters[0].value, json!(true));
        assert_eq!(compiled.parameters[1].value, json!(100));
    }

    #[test]
    fn literal_content_is_never_interpolated_into_sql() {
        let malicious = "' OR 1 = 1; DROP TABLE users; --";
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: column_ref("users", "name"),
            op: ComparisonOperator::Eq,
            right: literal(json!(malicious), DataType::String),
        });

        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("literal should be parameterized");
        assert!(!compiled.sql.contains(malicious));
        assert!(compiled.sql.ends_with("\"users\".\"name\" = $1"));
        assert_eq!(compiled.parameters[0].value, json!(malicious));
    }

    #[test]
    fn unsafe_function_name_is_rejected_instead_of_interpolated() {
        let mut plan = base_plan();
        plan.select = vec![Projection::Expr {
            expression: Expression::FunctionCall {
                name: "count); DROP TABLE users; --".to_owned(),
                args: vec![column_ref("users", "id")],
                distinct: false,
            },
            alias: None,
        }];

        let error = PostgresCompiler
            .compile(&validated(plan))
            .expect_err("unsafe function name must be rejected");
        assert_eq!(error.error_code(), "C001");
    }

    #[test]
    fn in_between_not_and_is_null_are_rendered_and_parameterized() {
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::And {
            left: Box::new(Predicate::Between {
                expr: column_ref("users", "id"),
                low: literal(json!(1), DataType::Int),
                high: literal(json!(10), DataType::Int),
            }),
            right: Box::new(Predicate::Or {
                left: Box::new(Predicate::In {
                    expr: column_ref("users", "id"),
                    target: InTarget::Values(vec![
                        literal(json!(2), DataType::Int),
                        literal(json!(3), DataType::Int),
                    ]),
                }),
                right: Box::new(Predicate::Not {
                    child: Box::new(Predicate::IsNull {
                        expr: column_ref("users", "name"),
                    }),
                }),
            }),
        });

        let compiled = SQLiteCompiler
            .compile(&validated(plan))
            .expect("predicate variants should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\" FROM \"users\" WHERE (\"users\".\"id\" BETWEEN ? AND ?) AND ((\"users\".\"id\" IN (?, ?)) OR (NOT (\"users\".\"name\" IS NULL)))"
        );
        assert_eq!(compiled.parameters.len(), 4);
        assert_eq!(
            compiled
                .parameters
                .iter()
                .map(|parameter| parameter.value.clone())
                .collect::<Vec<_>>(),
            vec![json!(1), json!(10), json!(2), json!(3)]
        );
    }

    #[test]
    fn query_builder_postgres_placeholders_renumber_per_invocation() {
        // Every call to `add_parameter` increments the placeholder
        // counter. `ColumnRef` expressions do not consume placeholders
        // (they render to a qualified identifier), so the third
        // expression to allocate a parameter must produce `$1`.
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: column_ref("users", "name"),
            op: ComparisonOperator::Eq,
            right: literal(json!("alice"), DataType::String),
        });
        let validated = validated(plan);
        let mut builder = QueryBuilder::new(
            &validated,
            SqlDialect::Postgres,
            IdentifierQuoting::DoubleQuote,
        );
        // Column references do not allocate parameters.
        let _ = builder.render_expression(&column_ref("users", "id"));
        let _ = builder.render_expression(&column_ref("users", "name"));
        // The first literal expression allocates the first parameter.
        let sql = builder
            .render_expression(&literal(json!("alice"), DataType::String))
            .expect("literal renders");
        assert_eq!(sql, "$1");
        // A second literal advances the counter.
        let sql = builder
            .render_expression(&literal(json!("bob"), DataType::String))
            .expect("literal renders");
        assert_eq!(sql, "$2");
    }

    #[test]
    fn query_builder_sqlite_reuses_single_placeholder_shape() {
        // SQLite and MySQL use `?` for every bind parameter, but the
        // parameter list still grows in textual order.
        let mut plan = base_plan();
        plan.r#where = Some(Predicate::Comparison {
            left: column_ref("users", "name"),
            op: ComparisonOperator::Eq,
            right: literal(json!("alice"), DataType::String),
        });
        plan.order_by = Some(vec![OrderByTerm {
            expr: literal(json!(1), DataType::Int),
            descending: true,
        }]);
        let validated = validated(plan);
        let compiled = SQLiteCompiler.compile(&validated).expect("sqlite compiles");
        // Two `?` placeholders, two parameters in textual order.
        assert!(compiled.sql.contains("\"users\".\"name\" = ?"));
        assert!(compiled.sql.contains("ORDER BY ? DESC"));
        assert_eq!(compiled.parameters.len(), 2);
        assert_eq!(compiled.parameters[0].value, json!("alice"));
        assert_eq!(compiled.parameters[1].value, json!(1));
    }

    #[test]
    fn query_builder_mysql_pagination_combines_offset_and_limit() {
        // MySQL uses `LIMIT offset, limit` syntax (note the reversed
        // argument order) and `?` for every parameter.
        let mut plan = base_plan();
        plan.limit = Some(50);
        plan.offset = Some(200);
        let validated = validated(plan);
        let compiled = MySQLCompiler.compile(&validated).expect("mysql compiles");
        assert!(compiled.sql.contains("LIMIT ?, ?"), "{}", compiled.sql);
        assert_eq!(compiled.parameters.len(), 2);
        // MySQL adds offset first, then limit
        assert_eq!(compiled.parameters[0].value, json!(200));
        assert_eq!(compiled.parameters[1].value, json!(50));
    }

    #[test]
    fn postgres_compiles_window_function_row_number() {
        let plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                    alias: None,
                },
                Projection::Expr {
                    expression: Expression::WindowFunction {
                        name: "ROW_NUMBER".to_owned(),
                        args: vec![],
                        distinct: false,
                        over: WindowSpec {
                            partition_by: Some(vec![column_ref("users", "name")]),
                            order_by: Some(vec![OrderByTerm {
                                expr: column_ref("users", "id"),
                                descending: true,
                            }]),
                            frame: None,
                        },
                    },
                    alias: Some("rn".to_owned()),
                },
            ],
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
            set_operation: None,
        };
        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("window function should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\", ROW_NUMBER() OVER (PARTITION BY \"users\".\"name\" ORDER BY \"users\".\"id\" DESC) AS \"rn\" FROM \"users\""
        );
    }

    #[test]
    fn postgres_compiles_window_function_with_frame() {
        let plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                    alias: None,
                },
                Projection::Expr {
                    expression: Expression::WindowFunction {
                        name: "SUM".to_owned(),
                        args: vec![column_ref("users", "id")],
                        distinct: false,
                        over: WindowSpec {
                            partition_by: None,
                            order_by: Some(vec![OrderByTerm {
                                expr: column_ref("users", "id"),
                                descending: false,
                            }]),
                            frame: Some(crate::schema::WindowFrame {
                                kind: crate::schema::WindowFrameKind::Rows,
                                start: crate::schema::WindowFrameBound::UnboundedPreceding,
                                end: Some(crate::schema::WindowFrameBound::CurrentRow),
                            }),
                        },
                    },
                    alias: Some("running_total".to_owned()),
                },
            ],
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
            set_operation: None,
        };
        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("window function with frame should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\", SUM(\"users\".\"id\") OVER (ORDER BY \"users\".\"id\" ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS \"running_total\" FROM \"users\""
        );
    }

    #[test]
    fn postgres_compiles_union_all() {
        let right = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                    alias: None,
                },
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "name".to_owned(),
                    alias: None,
                },
            ],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: Some(Predicate::Comparison {
                left: column_ref("users", "id"),
                op: ComparisonOperator::Gt,
                right: literal(json!(50), DataType::Int),
            }),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,
        };
        let plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                    alias: None,
                },
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "name".to_owned(),
                    alias: None,
                },
            ],
            from: FromClause {
                table: "users".to_owned(),
                alias: None,
            },
            r#where: Some(Predicate::Comparison {
                left: column_ref("users", "id"),
                op: ComparisonOperator::Lte,
                right: literal(json!(50), DataType::Int),
            }),
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: Some(SetOperationClause {
                operation: SetOperation::UnionAll,
                right: Box::new(right),
            }),
        };
        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("UNION ALL should compile");
        assert_eq!(
            compiled.sql,
            "SELECT \"users\".\"id\", \"users\".\"name\" FROM \"users\" WHERE \"users\".\"id\" <= $1 UNION ALL SELECT \"users\".\"id\", \"users\".\"name\" FROM \"users\" WHERE \"users\".\"id\" > $2"
        );
        assert_eq!(compiled.parameters.len(), 2);
    }

    #[test]
    fn postgres_compiles_select_distinct() {
        let plan = QueryPlan {
            select: vec![
                Projection::Column {
                    table: Some("users".to_owned()),
                    column: "name".to_owned(),
                    alias: None,
                },
            ],
            distinct: true,
            distinct_on: None,
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
            set_operation: None,
        };
        let compiled = PostgresCompiler
            .compile(&validated(plan))
            .expect("SELECT DISTINCT should compile");
        assert_eq!(
            compiled.sql,
            "SELECT DISTINCT \"users\".\"name\" FROM \"users\""
        );
    }
}
