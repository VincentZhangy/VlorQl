use super::*;
use crate::schema::{
    ComparisonOperator, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
    QueryPlan,
};
use crate::statistics::{
    ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
};
use std::collections::HashSet;

fn from(table: &str) -> FromClause {
    FromClause::table(table.to_owned(), None)
}

fn col(table: &str, column: &str) -> Expression {
    Expression::ColumnRef {
        table: Some(table.to_owned()),
        column: column.to_owned(),
    }
}

fn cmp(left: Expression, op: ComparisonOperator, right: Expression) -> Predicate {
    Predicate::Comparison { left: Box::new(left), op, right: Box::new(right) }
}

fn eq(left: Expression, right: Expression) -> Predicate {
    cmp(left, ComparisonOperator::Eq, right)
}

fn inner_join(table: &str, on: Predicate) -> JoinClause {
    JoinClause {
        join_type: JoinType::Inner,
        right_table: from(table),
        on,
    }
}

fn plan_with_joins(from_table: &str, joins: Vec<JoinClause>) -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: from(from_table),
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: Some(joins),
        ctes: None,
        distinct: false,
        distinct_on: None,
        set_operation: None,
    }
}

fn table(row_count: u64, columns: &[(&str, u64)]) -> TableStatistics {
    let mut stats = TableStatistics {
        row_count,
        ..TableStatistics::default()
    };
    for (name, distinct) in columns {
        stats.columns.insert(
            (*name).to_owned(),
            ColumnStatistics {
                distinct_count: *distinct,
                ..ColumnStatistics::default()
            },
        );
    }
    stats
}

fn reorderer(catalog: StatisticsCatalog) -> JoinReorderer {
    JoinReorderer::new(Arc::new(DummyStatisticsProvider::new(catalog)))
}

fn on_conjuncts(plan: &QueryPlan) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(joins) = &plan.joins {
        for join in joins {
            for pred in split_conjuncts(&join.on) {
                out.push(serde_json::to_string(&pred).expect("predicate serializes"));
            }
        }
    }
    out.sort();
    out
}

fn relation_names(plan: &QueryPlan) -> HashSet<String> {
    let mut names = HashSet::new();
    names.insert(plan.from.table_name().unwrap().to_owned());
    if let Some(joins) = &plan.joins {
        for join in joins {
            names.insert(join.right_table.table_name().unwrap().to_owned());
        }
    }
    names
}

#[tokio::test]
async fn three_table_join_starts_from_smallest_base_table() {
    let mut catalog = StatisticsCatalog::default();
    catalog
        .tables
        .insert("users".to_owned(), table(10_000, &[("id", 10_000)]));
    catalog.tables.insert(
        "orders".to_owned(),
        table(50_000, &[("user_id", 8_000), ("id", 50_000)]),
    );
    catalog
        .tables
        .insert("items".to_owned(), table(100, &[("order_id", 100)]));

    let plan = plan_with_joins(
        "users",
        vec![
            inner_join("orders", eq(col("users", "id"), col("orders", "user_id"))),
            inner_join("items", eq(col("orders", "id"), col("items", "order_id"))),
        ],
    );

    let reorderer = reorderer(catalog);
    let original_cost = reorderer.estimate_plan_cost(&plan).await.unwrap();
    let reordered = reorderer.reorder(&plan).await.unwrap();

    assert_eq!(reordered.from.table_name().unwrap(), "items");

    assert_eq!(relation_names(&reordered), relation_names(&plan));
    assert_eq!(on_conjuncts(&plan), on_conjuncts(&reordered));
    let joins = reordered.joins.as_ref().unwrap();
    assert_eq!(joins.len(), 2);
    assert!(joins.iter().all(|j| j.join_type == JoinType::Inner));

    let reordered_cost = reorderer.estimate_plan_cost(&reordered).await.unwrap();
    assert!(
        reordered_cost.total() < original_cost.total(),
        "expected {reordered_cost:?} < {original_cost:?}"
    );
}

#[tokio::test]
async fn non_equi_join_condition_degrades_to_default_selectivity() {
    let mut catalog = StatisticsCatalog::default();
    catalog
        .tables
        .insert("a".to_owned(), table(100, &[("x", 100)]));
    catalog.tables.insert(
        "b".to_owned(),
        table(50_000, &[("x", 40_000), ("y", 50_000)]),
    );
    catalog
        .tables
        .insert("c".to_owned(), table(10_000, &[("y", 10_000)]));

    let plan = plan_with_joins(
        "c",
        vec![
            inner_join(
                "b",
                cmp(col("b", "y"), ComparisonOperator::Gt, col("c", "y")),
            ),
            inner_join("a", eq(col("a", "x"), col("b", "x"))),
        ],
    );

    let reorderer = reorderer(catalog);
    let reordered = reorderer.reorder(&plan).await.unwrap();

    assert_eq!(on_conjuncts(&plan), on_conjuncts(&reordered));
    assert_eq!(relation_names(&reordered), relation_names(&plan));

    assert_eq!(reordered.from.table_name().unwrap(), "a");

    let original_cost = reorderer.estimate_plan_cost(&plan).await.unwrap();
    let reordered_cost = reorderer.estimate_plan_cost(&reordered).await.unwrap();
    assert!(
        reordered_cost.total() < original_cost.total(),
        "expected {reordered_cost:?} < {original_cost:?}"
    );
}

#[tokio::test]
async fn missing_statistics_fall_back_to_default_cardinality() {
    let plan = plan_with_joins(
        "users",
        vec![
            inner_join("orders", eq(col("users", "id"), col("orders", "user_id"))),
            inner_join("items", eq(col("orders", "id"), col("items", "order_id"))),
        ],
    );

    let reorderer = reorderer(StatisticsCatalog::default());
    let reordered = reorderer.reorder(&plan).await.unwrap();

    assert_eq!(reordered, plan);
}

#[tokio::test]
async fn left_join_is_left_unchanged() {
    let plan = plan_with_joins(
        "users",
        vec![JoinClause {
            join_type: JoinType::Left,
            right_table: from("orders"),
            on: eq(col("users", "id"), col("orders", "user_id")),
        }],
    );

    assert!(JoinGraph::build(&plan).is_none());
    let reorderer = reorderer(StatisticsCatalog::default());
    assert_eq!(reorderer.reorder(&plan).await.unwrap(), plan);
}

#[tokio::test]
async fn disconnected_join_graph_is_left_unchanged() {
    let plan = plan_with_joins(
        "users",
        vec![
            inner_join(
                "accounts",
                eq(col("users", "id"), col("accounts", "user_id")),
            ),
            inner_join("orders", eq(col("orders", "a"), col("orders", "b"))),
        ],
    );
    assert!(JoinGraph::build(&plan).is_none());
}
