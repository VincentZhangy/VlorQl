//! Typed schema and query-plan models.

pub mod dialect;
pub mod expressions;
pub mod query_plan;
pub mod snapshot;
pub mod types;

pub use dialect::{DialectProfile, DialectProfileBuilder};
pub use expressions::{Expression, InTarget, Predicate};
pub use query_plan::{
    CommonTableExpression, FromClause, JoinClause, OrderByTerm, Projection, QueryPlan,
};
pub use snapshot::{
    ArcSchemaSnapshot, ColumnSchema, ForeignKey, SchemaMetadata, SchemaSnapshot, TableSchema,
};
pub use types::{
    BinaryOperator, ComparisonOperator, DataType, IdentifierQuoting, JoinType, SqlDialect,
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};
    use std::sync::Arc;

    fn simple_plan() -> QueryPlan {
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
            r#where: Some(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "active".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: json!(true),
                    data_type: DataType::Boolean,
                },
            }),
            group_by: None,
            having: None,
            order_by: None,
            limit: Some(25),
            offset: None,
            joins: None,
            ctes: None,
        }
    }

    #[test]
    fn query_plan_round_trips_and_omits_missing_pagination_fields() {
        let plan = simple_plan();
        let value = to_value(&plan).expect("query plan should serialize");
        assert_eq!(value["limit"], 25);
        assert!(value.get("offset").is_none());

        let decoded: QueryPlan = from_value(value).expect("query plan should deserialize");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn query_plan_rejects_unknown_fields() {
        let mut value = to_value(simple_plan()).expect("query plan should serialize");
        value
            .as_object_mut()
            .expect("plan should be an object")
            .insert("unexpected".to_owned(), json!(true));

        let error = from_value::<QueryPlan>(value).expect_err("unknown fields must be rejected");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn tagged_expression_and_predicate_reject_unknown_fields() {
        let expression = json!({
            "type": "column_ref",
            "table": "users",
            "column": "id",
            "unexpected": true
        });
        let predicate = json!({
            "type": "is_null",
            "expr": {
                "type": "column_ref",
                "table": "users",
                "column": "deleted_at"
            },
            "unexpected": true
        });

        assert!(from_value::<Expression>(expression).is_err());
        assert!(from_value::<Predicate>(predicate).is_err());
    }

    #[test]
    fn schema_snapshot_uses_indexed_table_and_column_lookup() {
        let snapshot = SchemaSnapshot::new(
            vec![TableSchema {
                name: "users".to_owned(),
                columns: vec![ColumnSchema {
                    name: "id".to_owned(),
                    data_type: DataType::Uuid,
                    nullable: false,
                    description: None,
                    is_primary_key: true,
                    foreign_key: None,
                }],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            SchemaMetadata::default(),
        );

        assert_eq!(
            snapshot.get_table("users").map(|table| table.name.as_str()),
            Some("users")
        );
        assert_eq!(
            snapshot
                .get_column("users", "id")
                .map(|column| column.data_type),
            Some(DataType::Uuid)
        );
        assert!(snapshot.get_table("missing").is_none());
        assert!(snapshot.get_column("users", "missing").is_none());

        let serialized = to_value(&snapshot).expect("schema snapshot should serialize");
        assert!(serialized.get("table_index").is_none());
        assert!(serialized["tables"].is_array());

        let restored: SchemaSnapshot = from_value(serialized).expect("snapshot should deserialize");
        assert!(restored.get_table("users").is_some());
    }

    #[test]
    fn arc_schema_snapshot_shares_ownership() {
        let snapshot = Arc::new(SchemaSnapshot::default());
        let shared: ArcSchemaSnapshot = Arc::clone(&snapshot);
        assert_eq!(Arc::strong_count(&snapshot), 2);
        assert_eq!(shared.table_count(), 0);
    }

    #[test]
    fn dialect_profile_defaults_and_validates_features() {
        let profile = DialectProfile::default();
        assert_eq!(profile.dialect, SqlDialect::Postgres);
        assert_eq!(profile.quote_style, IdentifierQuoting::DoubleQuote);
        assert!(profile.supports_cte);
        assert!(profile.supports_offset);

        let restricted = DialectProfile::builder()
            .max_joins(1usize)
            .supports_offset(false)
            .build()
            .expect("builder should apply defaults");
        let mut plan = simple_plan();
        plan.offset = Some(10);
        let error = restricted
            .validate_dialect_features(&plan)
            .expect_err("offset should be rejected");
        assert_eq!(error.error_code(), "V007");

        plan.offset = None;
        plan.joins = Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "accounts".to_owned(),
                alias: None,
            },
            on: Predicate::IsNull {
                expr: Expression::ColumnRef {
                    table: Some("accounts".to_owned()),
                    column: "deleted_at".to_owned(),
                },
            },
        }]);
        let profile = DialectProfile::builder()
            .max_joins(0usize)
            .build()
            .expect("builder should apply defaults");
        let error = profile
            .validate_dialect_features(&plan)
            .expect_err("join count should be rejected");
        assert_eq!(error.error_code(), "V008");
    }

    #[test]
    fn query_plan_optional_fields_default_to_none_when_omitted() {
        // `where`, `group_by`, `having`, `order_by`, `limit`, `offset`,
        // `joins`, and `ctes` are all `#[serde(default)]`. A JSON body
        // that omits them must deserialize successfully and round-trip
        // to the same plan.
        let body = json!({
            "select": [{
                "type": "column",
                "table": "users",
                "column": "id",
                "alias": null
            }],
            "from": {"table": "users", "alias": null}
        });
        let plan: QueryPlan = from_value(body).expect("optional fields may be omitted");
        assert!(plan.r#where.is_none());
        assert!(plan.group_by.is_none());
        assert!(plan.having.is_none());
        assert!(plan.order_by.is_none());
        assert!(plan.limit.is_none());
        assert!(plan.offset.is_none());
        assert!(plan.joins.is_none());
        assert!(plan.ctes.is_none());

        let restored_value = to_value(&plan).expect("plan should serialize");
        // `limit` and `offset` use `skip_serializing_if = "Option::is_none"`.
        assert!(restored_value.get("limit").is_none());
        assert!(restored_value.get("offset").is_none());
    }

    #[test]
    fn query_plan_round_trip_preserves_limit_and_offset_when_set() {
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
            limit: Some(50),
            offset: Some(100),
            joins: None,
            ctes: None,
        };
        let value = to_value(&plan).expect("plan should serialize");
        assert_eq!(value["limit"], 50);
        assert_eq!(value["offset"], 100);
        let restored: QueryPlan = from_value(value).expect("plan should round-trip");
        assert_eq!(restored, plan);
    }

    #[test]
    fn from_clause_rejects_unknown_fields() {
        let body = json!({"table": "users", "alias": null, "sneaky": 1});
        let error = from_value::<FromClause>(body).expect_err("unknown FromClause field");
        assert!(error.to_string().contains("sneaky") || error.to_string().contains("unknown"));
    }

    #[test]
    fn schema_snapshot_lookup_helpers_distinguish_table_and_column() {
        let snapshot = SchemaSnapshot::new(
            vec![TableSchema {
                name: "users".to_owned(),
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
                        name: "email".to_owned(),
                        data_type: DataType::String,
                        nullable: true,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            SchemaMetadata::default(),
        );

        // Direct table lookup.
        assert_eq!(snapshot.table_count(), 1);
        assert_eq!(snapshot.get_table("users").unwrap().name, "users");
        assert!(snapshot.get_table("missing").is_none());

        // Column lookup.
        assert_eq!(
            snapshot.get_column("users", "id").map(|c| c.data_type),
            Some(DataType::Int)
        );
        assert_eq!(
            snapshot.get_column("users", "email").map(|c| c.nullable),
            Some(true)
        );
        // Unknown column on a known table.
        assert!(snapshot.get_column("users", "missing").is_none());
        // Unknown table always wins over column name.
        assert!(snapshot.get_column("missing", "id").is_none());
    }

    #[test]
    fn schema_snapshot_set_tables_replaces_index() {
        let mut snapshot = SchemaSnapshot::new(
            vec![TableSchema {
                name: "old".to_owned(),
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
            }],
            SchemaMetadata::default(),
        );
        assert!(snapshot.get_table("old").is_some());

        snapshot.set_tables(vec![TableSchema {
            name: "new".to_owned(),
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
        }]);
        assert!(snapshot.get_table("new").is_some());
        assert!(snapshot.get_table("old").is_none());
    }

    #[test]
    fn schema_snapshot_serializes_without_table_index() {
        let snapshot = SchemaSnapshot::default();
        let value = to_value(&snapshot).expect("snapshot should serialize");
        // The internal `table_index` is marked `#[serde(skip)]`.
        assert!(value.get("table_index").is_none());
        assert_eq!(value["tables"], json!([]));
    }

    #[test]
    fn dialect_profile_builder_accepts_all_fields() {
        let profile = DialectProfile::builder()
            .dialect(SqlDialect::Sqlite)
            .quote_style(IdentifierQuoting::Backtick)
            .supports_cte(false)
            .supports_window_functions(true)
            .supports_json_operations(false)
            .max_joins(3usize)
            .allowed_join_types(vec![JoinType::Inner, JoinType::Left])
            .allowed_functions(vec!["count".to_owned()])
            .denied_functions(vec!["load_extension".to_owned()])
            .max_group_by_columns(4usize)
            .allow_distinct(false)
            .supports_offset(false)
            .supports_fetch(false)
            .build()
            .expect("fully configured profile should build");
        assert_eq!(profile.dialect, SqlDialect::Sqlite);
        assert_eq!(profile.quote_style, IdentifierQuoting::Backtick);
        assert!(!profile.supports_cte);
        assert!(profile.supports_window_functions);
        assert!(!profile.supports_json_operations);
        assert_eq!(profile.max_joins, Some(3));
        assert_eq!(
            profile.allowed_join_types,
            vec![JoinType::Inner, JoinType::Left]
        );
        assert_eq!(profile.allowed_functions, vec!["count".to_owned()]);
        assert_eq!(profile.denied_functions, vec!["load_extension".to_owned()]);
        assert_eq!(profile.max_group_by_columns, Some(4));
        assert!(!profile.allow_distinct);
        assert!(!profile.supports_offset);
        assert!(!profile.supports_fetch);
    }

    #[test]
    fn dialect_profile_builder_fills_unspecified_fields_with_defaults() {
        let profile = DialectProfile::builder()
            .dialect(SqlDialect::Sqlite)
            .build()
            .expect("partial profile should build");
        // Every unspecified field must come from `DialectProfile::default()`.
        assert_eq!(profile.quote_style, IdentifierQuoting::DoubleQuote);
        assert!(profile.supports_cte);
        assert!(profile.supports_window_functions);
        assert!(profile.supports_json_operations);
        assert!(profile.max_joins.is_none());
        assert!(profile.max_group_by_columns.is_none());
        assert!(profile.allow_distinct);
        assert!(profile.supports_offset);
        assert!(profile.supports_fetch);
        assert_eq!(
            profile.allowed_join_types,
            DialectProfile::default().allowed_join_types
        );
        assert!(profile.allowed_functions.is_empty());
        assert!(profile.denied_functions.is_empty());
    }

    #[test]
    fn dialect_profile_default_has_postgres_shape() {
        let profile = DialectProfile::default();
        assert_eq!(profile.dialect, SqlDialect::Postgres);
        assert_eq!(profile.quote_style, IdentifierQuoting::DoubleQuote);
        assert!(profile.supports_cte);
        assert!(profile.supports_offset);
        assert!(profile.allow_distinct);
        assert_eq!(
            profile.allowed_join_types,
            vec![
                JoinType::Inner,
                JoinType::Left,
                JoinType::Right,
                JoinType::Full,
                JoinType::Cross,
            ]
        );
    }
}
