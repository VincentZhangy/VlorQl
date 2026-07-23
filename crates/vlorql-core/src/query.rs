use crate::schema::{Expression, FromClause, InTarget, JoinType, Predicate, Projection, QueryPlan};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(crate) struct QuerySource {
    pub(crate) table: String,
    pub(crate) alias: Option<String>,
}

impl From<&FromClause> for QuerySource {
    fn from(from: &FromClause) -> Self {
        Self {
            table: from.table.clone(),
            alias: from.alias.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct QueryScope {
    pub(crate) sources: Vec<QuerySource>,
    pub(crate) cte_names: HashSet<String>,
}

impl QueryScope {
    pub(crate) fn from_plan(plan: &QueryPlan) -> Self {
        let mut sources = Vec::with_capacity(1 + plan.joins.as_ref().map_or(0, Vec::len));
        sources.push(QuerySource::from(&plan.from));
        if let Some(joins) = &plan.joins {
            sources.extend(
                joins
                    .iter()
                    .map(|join| QuerySource::from(&join.right_table)),
            );
        }
        let cte_names = plan
            .ctes
            .as_ref()
            .into_iter()
            .flatten()
            .map(|cte| cte.name.clone())
            .collect();
        Self { sources, cte_names }
    }

    /// Merge outer scope sources into this scope.
    ///
    /// Correlated subqueries need access to the outer query's tables.
    /// Inner sources (from the subquery's own FROM/JOIN) take precedence
    /// over outer sources with the same table name.
    pub(crate) fn extend_with_outer(&mut self, outer: &QueryScope) {
        let inner_tables: HashSet<&str> = self.sources.iter().map(|s| s.table.as_str()).collect();
        let to_add: Vec<&QuerySource> = outer
            .sources
            .iter()
            .filter(|source| !inner_tables.contains(source.table.as_str()))
            .collect();
        for source in to_add {
            self.sources.push(source.clone());
        }
    }

    pub(crate) fn resolve_source(&self, qualifier: &str) -> Option<&QuerySource> {
        self.sources
            .iter()
            .find(|source| source.table == qualifier || source.alias.as_deref() == Some(qualifier))
    }
}

#[derive(Debug)]
pub(crate) struct ColumnReference {
    pub(crate) table: Option<String>,
    pub(crate) column: String,
}

#[derive(Debug)]
pub(crate) struct PlanReferences {
    pub(crate) columns: Vec<ColumnReference>,
    pub(crate) stars: Vec<Option<String>>,
}

pub(crate) fn collect_plan_references(plan: &QueryPlan) -> PlanReferences {
    let mut references = PlanReferences {
        columns: Vec::new(),
        stars: Vec::new(),
    };

    for projection in &plan.select {
        match projection {
            Projection::Column { table, column, .. } => {
                references.columns.push(ColumnReference {
                    table: table.clone(),
                    column: column.clone(),
                });
            }
            Projection::Expr { expression, .. } => {
                collect_expression_references(expression, &mut references.columns);
            }
            Projection::Star { table } => references.stars.push(table.clone()),
        }
    }
    if let Some(predicate) = &plan.r#where {
        collect_predicate_references(predicate, &mut references.columns);
    }
    if let Some(expressions) = &plan.group_by {
        for expression in expressions {
            collect_expression_references(expression, &mut references.columns);
        }
    }
    if let Some(predicate) = &plan.having {
        collect_predicate_references(predicate, &mut references.columns);
    }
    if let Some(terms) = &plan.order_by {
        for term in terms {
            collect_expression_references(&term.expr, &mut references.columns);
        }
    }
    if let Some(joins) = &plan.joins {
        for join in joins {
            // CROSS JOIN has no ON condition in SQL; the compiler ignores it.
            // Skip reference collection so hallucinated columns don't fail validation.
            if join.join_type != JoinType::Cross {
                collect_predicate_references(&join.on, &mut references.columns);
            }
        }
    }

    references
}

pub(crate) fn collect_expression_references(
    expression: &Expression,
    references: &mut Vec<ColumnReference>,
) {
    match expression {
        Expression::Literal { .. } => {}
        Expression::ColumnRef { table, column } => references.push(ColumnReference {
            table: table.clone(),
            column: column.clone(),
        }),
        Expression::FunctionCall { args, .. } => {
            for argument in args {
                collect_expression_references(argument, references);
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_expression_references(left, references);
            collect_expression_references(right, references);
        }
        Expression::Star => {}
        Expression::SubQuery { .. } => {}
        Expression::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_expression_references(op, references);
            }
            for wt in when_thens {
                collect_expression_references(&wt.when, references);
                collect_expression_references(&wt.then, references);
            }
            if let Some(el) = else_result {
                collect_expression_references(el, references);
            }
        }
        Expression::WindowFunction {
            args, over, ..
        } => {
            for argument in args {
                collect_expression_references(argument, references);
            }
            if let Some(partition_by) = &over.partition_by {
                for expr in partition_by {
                    collect_expression_references(expr, references);
                }
            }
            if let Some(order_by) = &over.order_by {
                for term in order_by {
                    collect_expression_references(&term.expr, references);
                }
            }
        }
    }
}

pub(crate) fn collect_predicate_references(
    predicate: &Predicate,
    references: &mut Vec<ColumnReference>,
) {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            collect_expression_references(left, references);
            collect_expression_references(right, references);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            collect_predicate_references(left, references);
            collect_predicate_references(right, references);
        }
        Predicate::Not { child } => collect_predicate_references(child, references),
        Predicate::Between { expr, low, high } => {
            collect_expression_references(expr, references);
            collect_expression_references(low, references);
            collect_expression_references(high, references);
        }
        Predicate::In { expr, target } => {
            collect_expression_references(expr, references);
            if let InTarget::Values(values) = target {
                for value in values {
                    collect_expression_references(value, references);
                }
            }
        }
        Predicate::Exists { .. } => {}
        Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
            collect_expression_references(expr, references);
        }
    }
}
