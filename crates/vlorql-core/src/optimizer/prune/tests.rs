use super::*;
use crate::schema::{
    BinaryOperator, ColumnSchema, ComparisonOperator, DataType, Expression, ForeignKey, FromClause,
    Predicate, Projection, SchemaMetadata, SchemaSnapshot, TableSchema,
};
use std::sync::Arc;

fn col(table: Option<&str>, column: &str) -> Expression {
    Expression::ColumnRef {
        table: table.map(str::to_owned),
        column: column.to_owned(),
    }
}

fn int(value: i64) -> Expression {
    Expression::Literal {
        value: value.into(),
        data_type: DataType::Int,
    }
}

fn lit_int(value: i64) -> Expression {
    int(value)
}

fn column_projection(table: Option<&str>, column: &str) -> Projection {
    Projection::Column {
        table: table.map(str::to_owned),
        column: column.to_owned(),
        alias: None,
    }
}

fn compare(left: Expression, op: ComparisonOperator, right: Expression) -> Predicate {
    Predicate::Comparison {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn eq(left: Expression, right: Expression) -> Predicate {
    compare(left, ComparisonOperator::Eq, right)
}

fn plan_with_cte(
    outer_select: Vec<Projection>,
    cte_name: &str,
    cte_projection: Vec<Projection>,
    cte_gb: Option<Vec<Expression>>,
    cte_having: Option<Predicate>,
    outer_where: Option<Predicate>,
) -> QueryPlan {
    let cte = CommonTableExpression {
        name: cte_name.to_owned(),
        recursive: false,
        query: Box::new(QueryPlan {
            select: cte_projection,
            from: FromClause::table("orders".to_owned(), None),
            r#where: None,
            group_by: cte_gb,
            having: cte_having,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }),
    };
    QueryPlan {
        select: outer_select,
        from: FromClause::table(cte_name.to_owned(), None),
        r#where: outer_where,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: Some(vec![cte]),
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

#[test]
fn prunes_unreferenced_cte_columns() {
    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "id")],
        "recent",
        vec![
            column_projection(Some("orders"), "id"),
            column_projection(Some("orders"), "user_id"),
            column_projection(Some("orders"), "status"),
        ],
        None,
        None,
        Some(eq(col(Some("recent"), "id"), lit_int(1))),
    );
    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    assert_eq!(cte_select.len(), 1);
    assert_eq!(cte_select[0], column_projection(Some("orders"), "id"));
}

#[test]
fn preserves_primary_and_foreign_keys() {
    let schema = Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "orders".to_owned(),
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
                    name: "user_id".to_owned(),
                    data_type: DataType::Int,
                    nullable: false,
                    description: None,
                    is_primary_key: false,
                    foreign_key: Some(ForeignKey {
                        foreign_table: "users".to_owned(),
                        foreign_column: "id".to_owned(),
                    }),
                },
                ColumnSchema {
                    name: "status".to_owned(),
                    data_type: DataType::Int,
                    nullable: false,
                    description: None,
                    is_primary_key: false,
                    foreign_key: None,
                },
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ));

    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "status")],
        "recent",
        vec![
            column_projection(Some("orders"), "id"),
            column_projection(Some("orders"), "user_id"),
            column_projection(Some("orders"), "status"),
        ],
        None,
        None,
        Some(eq(col(Some("recent"), "status"), lit_int(1))),
    );

    let rewritten = ColumnPruning::with_schema(schema).rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"status"), "referenced column kept");
    assert!(kept.contains(&"id"), "primary key preserved");
    assert!(kept.contains(&"user_id"), "foreign key preserved");
}

#[test]
fn keeps_all_columns_under_unqualified_reference() {
    let mut plan = plan_with_cte(
        vec![column_projection(None, "id")],
        "recent",
        vec![
            column_projection(Some("orders"), "id"),
            column_projection(Some("orders"), "user_id"),
            column_projection(Some("orders"), "status"),
        ],
        None,
        None,
        Some(eq(col(None, "status"), lit_int(1))),
    );
    plan.select = vec![column_projection(None, "id")];

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    assert_eq!(rewritten.ctes.as_ref().unwrap()[0].query.select.len(), 3);
}

#[test]
fn prunes_unused_columns_from_group_by_cte() {
    let plan = plan_with_cte(
        vec![
            column_projection(Some("recent"), "a"),
            column_projection(Some("recent"), "b"),
        ],
        "recent",
        vec![
            column_projection(Some("t"), "a"),
            column_projection(Some("t"), "b"),
            column_projection(Some("t"), "c"),
        ],
        Some(vec![col(None, "a"), col(None, "b")]),
        None,
        None,
    );

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"a"), "group key `a` kept");
    assert!(kept.contains(&"b"), "group key `b` kept");
    assert!(
        !kept.contains(&"c"),
        "`c` is neither key nor aggregate → pruned"
    );
}

#[test]
fn keeps_only_referenced_columns_from_group_by_cte() {
    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "a")],
        "recent",
        vec![
            column_projection(Some("t"), "a"),
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: Box::new(vec![col(None, "b")]),
                    distinct: false,
                },
                alias: Some("total".to_owned()),
            },
        ],
        Some(vec![col(None, "a")]),
        None,
        None,
    );

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            Projection::Expr { alias, .. } => alias.as_deref(),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"a"), "group key `a` kept");
    assert!(
        !kept.contains(&"total"),
        "unreferenced aggregate `total` pruned"
    );
}

#[test]
fn prunes_expression_projection_not_referenced_by_alias() {
    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "id")],
        "recent",
        vec![
            column_projection(Some("t"), "id"),
            Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(col(None, "col")),
                    op: BinaryOperator::Add,
                    right: Box::new(lit_int(1)),
                },
                alias: Some("plus_one".to_owned()),
            },
        ],
        None,
        None,
        None,
    );

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            Projection::Expr { alias, .. } => alias.as_deref(),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"id"), "referenced column `id` kept");
    assert!(
        !kept.contains(&"plus_one"),
        "unreferenced expression `plus_one` pruned"
    );
}

#[test]
fn keeps_expression_projection_when_referenced_by_alias() {
    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "plus_one")],
        "recent",
        vec![
            column_projection(Some("t"), "id"),
            Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(col(None, "col")),
                    op: BinaryOperator::Add,
                    right: Box::new(lit_int(1)),
                },
                alias: Some("plus_one".to_owned()),
            },
        ],
        None,
        None,
        None,
    );

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            Projection::Expr { alias, .. } => alias.as_deref(),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"plus_one"), "referenced expression kept");
    assert!(!kept.contains(&"id"), "unreferenced column `id` pruned");
}

#[test]
fn keeps_having_referenced_columns_in_group_by_cte() {
    let plan = plan_with_cte(
        vec![column_projection(Some("recent"), "a")],
        "recent",
        vec![
            column_projection(Some("t"), "a"),
            column_projection(Some("t"), "b"),
        ],
        Some(vec![col(None, "a"), col(None, "b")]),
        Some(compare(col(None, "b"), ComparisonOperator::Gt, lit_int(0))),
        None,
    );

    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select
        .iter()
        .filter_map(|p| match p {
            Projection::Column { column, .. } => Some(column.as_str()),
            _ => None,
        })
        .collect();

    assert!(kept.contains(&"a"), "group key `a` kept");
    assert!(kept.contains(&"b"), "HAVING-referenced `b` kept");
}
