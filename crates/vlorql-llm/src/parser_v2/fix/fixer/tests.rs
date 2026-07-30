use super::*;
use vlorql_core::schema::*;

fn base_plan() -> QueryPlan {
    QueryPlan {
        select: vec![Projection::Star { table: None }],
        from: FromClause::table("users".to_owned(), None),
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

#[test]
fn removes_limit_zero() {
    let mut plan = base_plan();
    plan.limit = Some(0);
    assert!(fix_limit_zero(&mut plan));
    assert_eq!(plan.limit, None);
}

#[test]
fn keeps_valid_limit() {
    let mut plan = base_plan();
    plan.limit = Some(10);
    assert!(!fix_limit_zero(&mut plan));
    assert_eq!(plan.limit, Some(10));
}

#[test]
fn keeps_none_limit() {
    let mut plan = base_plan();
    assert!(!fix_limit_zero(&mut plan));
    assert_eq!(plan.limit, None);
}

#[test]
fn injects_star_for_empty_select() {
    let mut plan = base_plan();
    plan.select = vec![];
    assert!(fix_empty_select(&mut plan));
    assert_eq!(plan.select.len(), 1);
    assert!(matches!(plan.select[0], Projection::Star { .. }));
}

#[test]
fn keeps_valid_select() {
    let mut plan = base_plan();
    assert!(!fix_empty_select(&mut plan));
    assert_eq!(plan.select.len(), 1);
}

#[test]
fn adds_alias_to_from() {
    let mut plan = base_plan();
    assert!(fix_missing_aliases(&mut plan));
    assert_eq!(plan.from.alias(), Some("t1".to_owned()));
}

#[test]
fn adds_alias_to_join() {
    let mut plan = base_plan();
    plan.joins = Some(vec![JoinClause {
        join_type: JoinType::Inner,
        right_table: FromClause::table("orders".to_owned(), None),
        on: Predicate::Comparison {
            left: Box::new(Expression::ColumnRef {
                table: None,
                column: "user_id".to_owned(),
            }),
            op: ComparisonOperator::Eq,
            right: Box::new(Expression::ColumnRef {
                table: None,
                column: "id".to_owned(),
            }),
        },
    }]);
    assert!(fix_missing_aliases(&mut plan));
    assert_eq!(plan.from.alias(), Some("t1".to_owned()));
    let join = &plan.joins.unwrap()[0];
    assert_eq!(join.right_table.alias(), Some("t2".to_owned()));
}

#[test]
fn skips_if_alias_already_exists() {
    let mut plan = base_plan();
    plan.from = FromClause::table("users".to_owned(), Some("u".to_owned()));
    assert!(!fix_missing_aliases(&mut plan));
    assert_eq!(plan.from.alias(), Some("u".to_owned()));
}

#[test]
fn generates_unique_aliases() {
    let mut plan = base_plan();
    plan.from = FromClause::table("users".to_owned(), Some("t1".to_owned()));
    plan.joins = Some(vec![JoinClause {
        join_type: JoinType::Inner,
        right_table: FromClause::table("orders".to_owned(), None),
        on: Predicate::Comparison {
            left: Box::new(Expression::ColumnRef {
                table: None,
                column: "user_id".to_owned(),
            }),
            op: ComparisonOperator::Eq,
            right: Box::new(Expression::ColumnRef {
                table: None,
                column: "id".to_owned(),
            }),
        },
    }]);
    assert!(fix_missing_aliases(&mut plan));
    let join = &plan.joins.unwrap()[0];
    assert_eq!(join.right_table.alias(), Some("t2".to_owned()));
}

#[test]
fn full_fix_pipeline() {
    let mut plan = base_plan();
    plan.limit = Some(0);
    plan.select = vec![];
    assert!(fix_plan(&mut plan));
    assert_eq!(plan.limit, None);
    assert_eq!(plan.select.len(), 1);
    assert_eq!(plan.from.alias(), Some("t1".to_owned()));
}

#[test]
fn no_change_for_valid_plan() {
    let mut plan = base_plan();
    plan.from = FromClause::table("users".to_owned(), Some("u".to_owned()));
    assert!(!fix_plan(&mut plan));
}

#[test]
fn apply_fixes_returns_new_plan() {
    let mut plan = base_plan();
    plan.limit = Some(0);
    let fixed = apply_fixes(plan);
    assert_eq!(fixed.limit, None);
    assert_eq!(fixed.from.alias(), Some("t1".to_owned()));
}

#[test]
fn fixes_cte_subquery() {
    let mut plan = base_plan();
    plan.ctes = Some(vec![CommonTableExpression {
        name: "active".to_owned(),
        recursive: false,
        query: Box::new(QueryPlan {
            select: vec![Projection::Star { table: None }],
            from: FromClause::table("users".to_owned(), None),
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: Some(0),
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }),
    }]);
    assert!(fix_plan(&mut plan));
    let cte = &plan.ctes.unwrap()[0];
    assert_eq!(cte.query.limit, None);
    assert_eq!(cte.query.from.alias(), Some("t1".to_owned()));
}
