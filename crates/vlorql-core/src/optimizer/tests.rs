use super::*;
use crate::schema::{
    BinaryOperator, ColumnSchema, CommonTableExpression, ComparisonOperator, DataType,
    Expression, ForeignKey, FromClause, JoinClause, JoinType, Predicate, Projection, QueryPlan,
    SchemaMetadata, SchemaSnapshot, TableSchema,
};
use std::sync::Arc;

fn col(table: Option<&str>, column: &str) -> Expression {
    Expression::ColumnRef { table: table.map(str::to_owned), column: column.to_owned() }
}

fn int(value: i64) -> Expression {
    Expression::Literal { value: value.into(), data_type: DataType::Int }
}

fn column_projection(table: Option<&str>, column: &str) -> Projection {
    Projection::Column { table: table.map(str::to_owned), column: column.to_owned(), alias: None }
}

fn compare(left: Expression, op: ComparisonOperator, right: Expression) -> Predicate {
    Predicate::Comparison { left: Box::new(left), op, right: Box::new(right) }
}

fn and(left: Predicate, right: Predicate) -> Predicate {
    Predicate::And { left: Box::new(left), right: Box::new(right) }
}

fn conjunct_count(pred: &Predicate) -> usize {
    match pred {
        Predicate::And { left, right } => conjunct_count(left) + conjunct_count(right),
        _ => 1,
    }
}

fn plan_with_cte(outer_where: Predicate) -> QueryPlan {
    let cte = CommonTableExpression {
        name: "recent".to_owned(),
        recursive: false,
        query: Box::new(QueryPlan {
            select: vec![
                column_projection(Some("orders"), "id"),
                column_projection(Some("orders"), "user_id"),
                column_projection(Some("orders"), "status"),
            ],
            from: FromClause::table("orders".to_owned(), None),
            r#where: None,
            group_by: None, having: None, order_by: None, limit: None, offset: None,
            joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
        }),
    };
    QueryPlan {
        select: vec![column_projection(Some("recent"), "id")],
        from: FromClause::table("recent".to_owned(), None),
        r#where: Some(outer_where),
        group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: Some(vec![cte]), distinct: false, distinct_on: None, set_operation: None,
    }
}

// --- constant folding ----------------------------------------------

#[test]
fn constant_folding_evaluates_arithmetic_in_projection() {
    let plan = QueryPlan {
        select: vec![Projection::Expr {
            expression: Expression::BinaryOp {
                left: Box::new(int(1)), op: BinaryOperator::Add, right: Box::new(int(2)),
            },
            alias: Some("three".to_owned()),
        }],
        from: FromClause::table("t".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let folded = ConstantFolding.rewrite(&plan).unwrap();
    assert_eq!(folded.select[0], Projection::Expr { expression: int(3), alias: Some("three".to_owned()) });
}

#[test]
fn constant_folding_simplifies_constant_side_of_comparison() {
    let plan = QueryPlan {
        select: vec![column_projection(Some("users"), "age")],
        from: FromClause::table("users".to_owned(), None),
        r#where: Some(compare(
            col(Some("users"), "age"), ComparisonOperator::Gt,
            Expression::BinaryOp { left: Box::new(int(20)), op: BinaryOperator::Add, right: Box::new(int(5)) },
        )),
        group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let folded = ConstantFolding.rewrite(&plan).unwrap();
    assert_eq!(folded.r#where, Some(compare(col(Some("users"), "age"), ComparisonOperator::Gt, int(25))));
}

#[test]
fn constant_folding_leaves_column_expressions_untouched() {
    let expr = Expression::BinaryOp {
        left: Box::new(col(Some("users"), "age")), op: BinaryOperator::Add, right: Box::new(int(1)),
    };
    let plan = QueryPlan {
        select: vec![Projection::Expr { expression: expr.clone(), alias: None }],
        from: FromClause::table("users".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let folded = ConstantFolding.rewrite(&plan).unwrap();
    assert_eq!(folded.select[0], Projection::Expr { expression: expr, alias: None });
}

// --- predicate pushdown --------------------------------------------

#[test]
fn pushdown_moves_single_cte_conjunct_into_the_cte() {
    let outer = and(
        compare(col(Some("recent"), "status"), ComparisonOperator::Eq, int(1)),
        compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(100)),
    );
    let plan = plan_with_cte(outer);
    let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
    assert!(rewritten.r#where.is_none());
    let cte_where = rewritten.ctes.as_ref().unwrap()[0].query.r#where.as_ref()
        .expect("CTE should have received the pushed conjuncts");
    assert_eq!(conjunct_count(cte_where), 2);
    let pushed = super::analyze::split_conjuncts(cte_where);
    for conjunct in &pushed {
        for (table, _) in super::analyze::columns_in_predicate(conjunct) {
            assert!(table.is_none(), "qualifier should be stripped inside CTE");
        }
    }
}

#[test]
fn pushdown_keeps_conjuncts_over_base_tables() {
    let plan = QueryPlan {
        select: vec![column_projection(Some("users"), "id")],
        from: FromClause::table("users".to_owned(), None),
        r#where: Some(compare(col(Some("users"), "active"), ComparisonOperator::Eq, int(1))),
        group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
    assert_eq!(rewritten, plan, "no CTE means the plan is unchanged");
}

#[test]
fn pushdown_reduces_outer_conjunct_count() {
    let outer = and(
        compare(col(Some("recent"), "status"), ComparisonOperator::Eq, int(1)),
        compare(col(Some("other"), "flag"), ComparisonOperator::Eq, int(1)),
    );
    let mut plan = plan_with_cte(outer);
    plan.joins = Some(vec![JoinClause {
        join_type: JoinType::Inner,
        right_table: FromClause::table("other".to_owned(), None),
        on: compare(col(Some("recent"), "user_id"), ComparisonOperator::Eq, col(Some("other"), "user_id")),
    }]);
    let before = conjunct_count(plan.r#where.as_ref().unwrap());
    let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
    let after = conjunct_count(rewritten.r#where.as_ref().unwrap());
    assert_eq!(before, 2);
    assert_eq!(after, 1, "the CTE conjunct should have moved out");
    assert!(rewritten.ctes.as_ref().unwrap()[0].query.r#where.is_some());
}

// --- column pruning ------------------------------------------------

#[test]
fn pruning_drops_unreferenced_cte_columns() {
    let plan = plan_with_cte(compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(0)));
    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    assert_eq!(cte_select.len(), 1);
    assert_eq!(cte_select[0], column_projection(Some("orders"), "id"));
}

#[test]
fn pruning_preserves_primary_and_foreign_keys() {
    let schema = Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "orders".to_owned(),
            columns: vec![
                ColumnSchema { name: "id".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: true, foreign_key: None },
                ColumnSchema { name: "user_id".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: false, foreign_key: Some(ForeignKey { foreign_table: "users".to_owned(), foreign_column: "id".to_owned() }) },
                ColumnSchema { name: "status".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: false, foreign_key: None },
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ));
    let mut plan = plan_with_cte(compare(col(Some("recent"), "status"), ComparisonOperator::Eq, int(1)));
    plan.select = vec![column_projection(Some("recent"), "status")];
    let rewritten = ColumnPruning::with_schema(schema).rewrite(&plan).unwrap();
    let cte_select = &rewritten.ctes.as_ref().unwrap()[0].query.select;
    let kept: Vec<&str> = cte_select.iter().filter_map(|p| match p { Projection::Column { column, .. } => Some(column.as_str()), _ => None }).collect();
    assert!(kept.contains(&"status"), "referenced column kept");
    assert!(kept.contains(&"id"), "primary key preserved");
    assert!(kept.contains(&"user_id"), "foreign key preserved");
}

#[test]
fn pruning_keeps_all_columns_under_unqualified_reference() {
    let mut plan = plan_with_cte(compare(col(None, "status"), ComparisonOperator::Eq, int(1)));
    plan.select = vec![column_projection(None, "id")];
    let rewritten = ColumnPruning::new().rewrite(&plan).unwrap();
    assert_eq!(rewritten.ctes.as_ref().unwrap()[0].query.select.len(), 3);
}

// --- pipeline ------------------------------------------------------

#[test]
fn pipeline_applies_rules_in_order() {
    let outer = and(
        compare(col(Some("recent"), "id"), ComparisonOperator::Gt,
            Expression::BinaryOp { left: Box::new(int(100)), op: BinaryOperator::Add, right: Box::new(int(0)) }),
        compare(col(Some("recent"), "status"), ComparisonOperator::Eq, int(1)),
    );
    let plan = plan_with_cte(outer);
    let pipeline = RewriterPipeline::new().with(ConstantFolding).with(PredicatePushdown).with(ColumnPruning::new());
    let rewritten = pipeline.rewrite(&plan).unwrap();
    assert!(rewritten.r#where.is_none());
    let cte = &rewritten.ctes.as_ref().unwrap()[0].query;
    let cte_where = cte.r#where.as_ref().expect("conjuncts pushed into CTE");
    assert_eq!(conjunct_count(cte_where), 2);
    assert!(super::analyze::split_conjuncts(cte_where).iter().any(|p| {
        matches!(p, Predicate::Comparison { right, .. } if **right == int(100))
    }));
    assert!(cte.select.len() < 3, "at least one column pruned");
}

#[test]
fn empty_pipeline_is_identity() {
    let plan = plan_with_cte(compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(0)));
    let pipeline = RewriterPipeline::new();
    assert!(pipeline.is_empty());
    assert_eq!(pipeline.rewrite(&plan).unwrap(), plan);
}

// --- QueryOptimizer orchestrator -------------------------------------

#[test]
fn query_optimizer_folds_constants() {
    let plan = QueryPlan {
        select: vec![Projection::Expr {
            expression: Expression::BinaryOp {
                left: Box::new(int(20)), op: BinaryOperator::Add, right: Box::new(int(5)),
            },
            alias: Some("total".to_owned()),
        }],
        from: FromClause::table("t".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let optimizer = QueryOptimizer::rewrites_only();
    let rewritten = optimizer.optimize(&plan).unwrap();
    assert_eq!(rewritten.select[0], Projection::Expr { expression: int(25), alias: Some("total".to_owned()) });
}

#[tokio::test]
async fn query_optimizer_async_runs_pipeline() {
    let plan = plan_with_cte(compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(0)));
    let optimizer = QueryOptimizer::rewrites_only();
    let rewritten = optimizer.optimize_async(&plan).await.unwrap();
    assert!(!rewritten.select.is_empty());
}

#[tokio::test]
async fn query_optimizer_with_stats_creates_join_reorderer() {
    use crate::statistics::DummyStatisticsProvider;
    let stats = Arc::new(DummyStatisticsProvider::default());
    let optimizer = QueryOptimizer::new(stats);
    let plan = QueryPlan {
        select: vec![Projection::Column { table: None, column: "id".to_owned(), alias: None }],
        from: FromClause::table("users".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let rewritten = optimizer.optimize_async(&plan).await.unwrap();
    assert_eq!(rewritten.from.table_name().unwrap(), "users");
}

// --- Security: pushdown must not push policy row filters into CTEs ---

#[test]
fn pushdown_does_not_push_policy_row_filter_into_cte() {
    let policy_filter = compare(col(None, "tenant_id"), ComparisonOperator::Eq, int(42));
    let plan = plan_with_cte(and(policy_filter.clone(), compare(col(Some("recent"), "id"), ComparisonOperator::Gt, int(0))));
    let rewritten = PredicatePushdown.rewrite(&plan).unwrap();
    let outer_where = rewritten.r#where.as_ref().unwrap();
    let conjuncts = crate::optimizer::analyze::split_conjuncts(outer_where);
    let has_policy_filter = conjuncts.iter().any(|c| matches!(c, Predicate::Comparison { right, .. } if **right == int(42)));
    assert!(has_policy_filter, "policy filter `tenant_id = 42` must remain in the outer WHERE");
}

// --- Security: column pruning must preserve PK/FK columns ---

#[test]
fn column_pruning_preserves_primary_and_foreign_keys() {
    use crate::schema::ForeignKey;
    let schema = Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "orders".to_owned(),
            columns: vec![
                ColumnSchema { name: "id".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: true, foreign_key: None },
                ColumnSchema { name: "user_id".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: false, foreign_key: Some(ForeignKey { foreign_table: "users".to_owned(), foreign_column: "id".to_owned() }) },
                ColumnSchema { name: "product_id".to_owned(), data_type: DataType::Int, nullable: false, description: None, is_primary_key: false, foreign_key: Some(ForeignKey { foreign_table: "products".to_owned(), foreign_column: "id".to_owned() }) },
                ColumnSchema { name: "status".to_owned(), data_type: DataType::String, nullable: false, description: None, is_primary_key: false, foreign_key: None },
            ],
            description: None,
            primary_key: Some(vec!["id".to_owned()]),
        }],
        SchemaMetadata::default(),
    ));
    let cte = CommonTableExpression {
        name: "recent".to_owned(), recursive: false,
        query: Box::new(QueryPlan {
            select: vec![
                column_projection(Some("orders"), "id"),
                column_projection(Some("orders"), "user_id"),
                column_projection(Some("orders"), "product_id"),
                column_projection(Some("orders"), "status"),
            ],
            from: FromClause::table("orders".to_owned(), None),
            r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
            joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
        }),
    };
    let plan = QueryPlan {
        select: vec![column_projection(Some("recent"), "status")],
        from: FromClause::table("recent".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: Some(vec![cte]), distinct: false, distinct_on: None, set_operation: None,
    };
    let pruner = ColumnPruning::with_schema(schema);
    let rewritten = pruner.rewrite(&plan).unwrap();
    let cte_cols: Vec<&str> = rewritten.ctes.as_ref().unwrap()[0].query.select.iter()
        .filter_map(|p| match p { Projection::Column { column, .. } => Some(column.as_str()), _ => None }).collect();
    assert!(cte_cols.contains(&"id"), "PK column `id` must be preserved: got {cte_cols:?}");
    assert!(cte_cols.contains(&"user_id"), "FK column `user_id` must be preserved: got {cte_cols:?}");
    assert!(cte_cols.contains(&"product_id"), "FK column `product_id` must be preserved: got {cte_cols:?}");
    assert!(cte_cols.contains(&"status"), "selected column `status` must be preserved: got {cte_cols:?}");
}

#[test]
fn repeat_until_stable_converges_in_one_round_when_already_stable() {
    let plan = QueryPlan {
        select: vec![column_projection(None, "id")],
        from: FromClause::table("t".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let pipeline = RewriterPipeline::new().with(ConstantFolding).with(PredicatePushdown).with(ColumnPruning::new());
    let result = pipeline.repeat_until_stable(&plan, 5).unwrap();
    assert_eq!(result.select.len(), 1);
}

#[test]
fn repeat_until_stable_preserves_equivalence() {
    let plan = QueryPlan {
        select: vec![Projection::Expr {
            expression: Expression::BinaryOp { left: Box::new(int(10)), op: BinaryOperator::Add, right: Box::new(int(20)) },
            alias: Some("total".to_owned()),
        }],
        from: FromClause::table("t".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let pipeline = RewriterPipeline::new().with(ConstantFolding);
    let result = pipeline.repeat_until_stable(&plan, 3).unwrap();
    assert_eq!(result.select[0], Projection::Expr { expression: int(30), alias: Some("total".to_owned()) });
}

#[test]
fn optimize_repeat_exposes_fixpoint_method() {
    let plan = QueryPlan {
        select: vec![column_projection(None, "id")],
        from: FromClause::table("t".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let optimizer = QueryOptimizer::rewrites_only();
    let result = optimizer.optimize_repeat(&plan, 3).unwrap();
    assert_eq!(result.select.len(), 1);
}

#[test]
fn multi_layer_cte_pushdown_cascades_through_nested_ctes() {
    let cte2_body = QueryPlan {
        select: vec![column_projection(None, "id"), column_projection(None, "val")],
        from: FromClause::table("t2".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let cte1_body = QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause::table("cte2".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let plan = QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause::table("cte1".to_owned(), None),
        r#where: Some(compare(col(Some("cte1"), "val"), ComparisonOperator::Gt, int(10))),
        group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None,
        ctes: Some(vec![
            CommonTableExpression { name: "cte2".to_owned(), recursive: false, query: Box::new(cte2_body) },
            CommonTableExpression { name: "cte1".to_owned(), recursive: false, query: Box::new(cte1_body) },
        ]),
        distinct: false, distinct_on: None, set_operation: None,
    };
    let pipeline = RewriterPipeline::new().with(PredicatePushdown);
    let result = pipeline.rewrite(&plan).unwrap();
    assert!(result.r#where.is_none(), "outer WHERE should be empty after pushdown");
    let cte1 = result.ctes.as_ref().unwrap().iter().find(|cte| cte.name == "cte1").expect("cte1 should exist");
    assert!(cte1.query.r#where.is_none(), "cte1 WHERE should be empty after cascade pushdown");
    let cte2 = result.ctes.as_ref().unwrap().iter().find(|cte| cte.name == "cte2").expect("cte2 should exist");
    assert!(cte2.query.r#where.is_some(), "cte2 should have the pushed condition");
}

#[test]
fn multi_layer_cte_pushdown_with_alias() {
    let cte2_body = QueryPlan {
        select: vec![column_projection(None, "id"), column_projection(None, "val")],
        from: FromClause::table("t2".to_owned(), None),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let cte1_body = QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause::table("cte2".to_owned(), Some("inner_c".to_owned())),
        r#where: None, group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
    };
    let plan = QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause::table("cte1".to_owned(), None),
        r#where: Some(compare(col(Some("cte1"), "val"), ComparisonOperator::Gt, int(10))),
        group_by: None, having: None, order_by: None, limit: None, offset: None,
        joins: None,
        ctes: Some(vec![
            CommonTableExpression { name: "cte2".to_owned(), recursive: false, query: Box::new(cte2_body) },
            CommonTableExpression { name: "cte1".to_owned(), recursive: false, query: Box::new(cte1_body) },
        ]),
        distinct: false, distinct_on: None, set_operation: None,
    };
    let pipeline = RewriterPipeline::new().with(PredicatePushdown);
    let result = pipeline.rewrite(&plan).unwrap();
    assert!(result.r#where.is_none(), "outer WHERE should be empty");
    let cte1 = result.ctes.as_ref().unwrap().iter().find(|cte| cte.name == "cte1").expect("cte1 should exist");
    assert!(cte1.query.r#where.is_none(), "cte1 WHERE should be empty after cascade");
    let cte2 = result.ctes.as_ref().unwrap().iter().find(|cte| cte.name == "cte2").expect("cte2 should exist");
    assert!(cte2.query.r#where.is_some(), "cte2 should have the pushed condition");
}
