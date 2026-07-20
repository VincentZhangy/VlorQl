//! Shared tree-traversal infrastructure for rewrite rules.
//!
//! Two patterns are provided:
//!
//! * **`ExpressionFold`** — transform an expression/predicate/plan tree
//!   by recursively rebuilding nodes. The default implementations are
//!   the identity (every node is cloned unchanged); override specific
//!   methods to apply a transformation.
//!
//! * **`ExpressionVisit`** — read-only traversal that collects
//!   information. The default implementations recurse into every child;
//!   override `visit_expression` / `visit_predicate` / `visit_plan` to
//!   observe specific nodes.
//!
//! Both patterns eliminate the duplicated recursion that would
//! otherwise appear in every rule.

use crate::schema::{
    CommonTableExpression, Expression, InTarget, JoinClause, OrderByTerm, Predicate, Projection,
    QueryPlan,
};

// ---------------------------------------------------------------------------
// Fold (transform) pattern
// ---------------------------------------------------------------------------

/// Transforms an expression/predicate/plan tree by recursively
/// rebuilding nodes. The default implementation for every method is the
/// identity — call one of the [`default_*`] functions inside your
/// override to recurse into children.
pub trait ExpressionFold {
    fn fold_expression(&mut self, expr: &Expression) -> Expression {
        default_fold_expression(self, expr)
    }

    fn fold_predicate(&mut self, pred: &Predicate) -> Predicate {
        default_fold_predicate(self, pred)
    }

    fn fold_projection(&mut self, proj: &Projection) -> Projection {
        default_fold_projection(self, proj)
    }

    fn fold_plan(&mut self, plan: &QueryPlan) -> QueryPlan {
        default_fold_plan(self, plan)
    }
}

impl<T: ExpressionFold + ?Sized> ExpressionFold for &mut T {
    fn fold_expression(&mut self, expr: &Expression) -> Expression {
        T::fold_expression(self, expr)
    }
    fn fold_predicate(&mut self, pred: &Predicate) -> Predicate {
        T::fold_predicate(self, pred)
    }
    fn fold_projection(&mut self, proj: &Projection) -> Projection {
        T::fold_projection(self, proj)
    }
    fn fold_plan(&mut self, plan: &QueryPlan) -> QueryPlan {
        T::fold_plan(self, plan)
    }
}

// -- default fold implementations ------------------------------------------

pub fn default_fold_expression<F: ExpressionFold + ?Sized>(
    folder: &mut F,
    expr: &Expression,
) -> Expression {
    match expr {
        Expression::BinaryOp { left, op, right } => Expression::BinaryOp {
            left: Box::new(folder.fold_expression(left)),
            op: *op,
            right: Box::new(folder.fold_expression(right)),
        },
        Expression::FunctionCall { name, args, distinct } => Expression::FunctionCall {
            name: name.clone(),
            args: args.iter().map(|a| folder.fold_expression(a)).collect(),
            distinct: *distinct,
        },
        Expression::SubQuery { query } => Expression::SubQuery {
            query: Box::new(folder.fold_plan(query)),
        },
        // Literals, column references, and Star are leaves.
        other => other.clone(),
    }
}

pub fn default_fold_predicate<F: ExpressionFold + ?Sized>(
    folder: &mut F,
    pred: &Predicate,
) -> Predicate {
    match pred {
        Predicate::Comparison { left, op, right } => Predicate::Comparison {
            left: folder.fold_expression(left),
            op: *op,
            right: folder.fold_expression(right),
        },
        Predicate::And { left, right } => Predicate::And {
            left: Box::new(folder.fold_predicate(left)),
            right: Box::new(folder.fold_predicate(right)),
        },
        Predicate::Or { left, right } => Predicate::Or {
            left: Box::new(folder.fold_predicate(left)),
            right: Box::new(folder.fold_predicate(right)),
        },
        Predicate::Not { child } => Predicate::Not {
            child: Box::new(folder.fold_predicate(child)),
        },
        Predicate::Between { expr, low, high } => Predicate::Between {
            expr: folder.fold_expression(expr),
            low: folder.fold_expression(low),
            high: folder.fold_expression(high),
        },
        Predicate::In { expr, target } => Predicate::In {
            expr: folder.fold_expression(expr),
            target: match target {
                InTarget::Values(values) => {
                    InTarget::Values(values.iter().map(|v| folder.fold_expression(v)).collect())
                }
                InTarget::SubQuery(query) => InTarget::SubQuery(query.clone()),
            },
        },
        Predicate::Exists { query } => Predicate::Exists {
            query: query.clone(),
        },
        Predicate::Like { expr, pattern } => Predicate::Like {
            expr: folder.fold_expression(expr),
            pattern: pattern.clone(),
        },
        Predicate::IsNull { expr } => Predicate::IsNull {
            expr: folder.fold_expression(expr),
        },
    }
}

pub fn default_fold_projection<F: ExpressionFold + ?Sized>(
    folder: &mut F,
    proj: &Projection,
) -> Projection {
    match proj {
        Projection::Column { table, column, alias } => Projection::Column {
            table: table.clone(),
            column: column.clone(),
            alias: alias.clone(),
        },
        Projection::Expr { expression, alias } => Projection::Expr {
            expression: folder.fold_expression(expression),
            alias: alias.clone(),
        },
        Projection::Star { table } => Projection::Star {
            table: table.clone(),
        },
    }
}

pub fn default_fold_plan<F: ExpressionFold + ?Sized>(
    folder: &mut F,
    plan: &QueryPlan,
) -> QueryPlan {
    QueryPlan {
        select: plan
            .select
            .iter()
            .map(|p| folder.fold_projection(p))
            .collect(),
        from: plan.from.clone(),
        r#where: plan.r#where.as_ref().map(|p| folder.fold_predicate(p)),
        group_by: plan
            .group_by
            .as_ref()
            .map(|exprs| exprs.iter().map(|e| folder.fold_expression(e)).collect()),
        having: plan.having.as_ref().map(|p| folder.fold_predicate(p)),
        order_by: plan.order_by.as_ref().map(|terms| {
            terms
                .iter()
                .map(|t| OrderByTerm {
                    expr: folder.fold_expression(&t.expr),
                    descending: t.descending,
                })
                .collect()
        }),
        limit: plan.limit,
        offset: plan.offset,
        joins: plan.joins.as_ref().map(|joins| {
            joins
                .iter()
                .map(|j| JoinClause {
                    join_type: j.join_type,
                    right_table: j.right_table.clone(),
                    on: folder.fold_predicate(&j.on),
                })
                .collect()
        }),
        ctes: plan.ctes.as_ref().map(|ctes| {
            ctes.iter()
                .map(|cte| CommonTableExpression {
                    name: cte.name.clone(),
                    query: Box::new(folder.fold_plan(&cte.query)),
                })
                .collect()
        }),
    }
}

// ---------------------------------------------------------------------------
// Visit (read-only) pattern
// ---------------------------------------------------------------------------

/// Read-only traversal over an expression/predicate/plan tree. The
/// default implementation of every method recurses into every child.
/// Override specific methods to observe nodes of interest.
pub trait ExpressionVisit {
    type Ctx;

    fn visit_expression(&mut self, expr: &Expression, ctx: &mut Self::Ctx) {
        default_visit_expression(self, expr, ctx)
    }

    fn visit_predicate(&mut self, pred: &Predicate, ctx: &mut Self::Ctx) {
        default_visit_predicate(self, pred, ctx)
    }

    fn visit_projection(&mut self, proj: &Projection, ctx: &mut Self::Ctx) {
        default_visit_projection(self, proj, ctx)
    }

    fn visit_plan(&mut self, plan: &QueryPlan, ctx: &mut Self::Ctx) {
        default_visit_plan(self, plan, ctx)
    }
}

// -- default visit implementations -----------------------------------------

pub fn default_visit_expression<F: ExpressionVisit + ?Sized>(
    visitor: &mut F,
    expr: &Expression,
    ctx: &mut F::Ctx,
) {
    match expr {
        Expression::BinaryOp { left, right, .. } => {
            visitor.visit_expression(left, ctx);
            visitor.visit_expression(right, ctx);
        }
        Expression::FunctionCall { args, .. } => {
            for arg in args {
                visitor.visit_expression(arg, ctx);
            }
        }
        Expression::SubQuery { query } => visitor.visit_plan(query, ctx),
        Expression::ColumnRef { .. } | Expression::Literal { .. } | Expression::Star => {}
    }
}

pub fn default_visit_predicate<F: ExpressionVisit + ?Sized>(
    visitor: &mut F,
    pred: &Predicate,
    ctx: &mut F::Ctx,
) {
    match pred {
        Predicate::Comparison { left, right, .. } => {
            visitor.visit_expression(left, ctx);
            visitor.visit_expression(right, ctx);
        }
        Predicate::And { left, right } | Predicate::Or { left, right } => {
            visitor.visit_predicate(left, ctx);
            visitor.visit_predicate(right, ctx);
        }
        Predicate::Not { child } => visitor.visit_predicate(child, ctx),
        Predicate::Between { expr, low, high } => {
            visitor.visit_expression(expr, ctx);
            visitor.visit_expression(low, ctx);
            visitor.visit_expression(high, ctx);
        }
        Predicate::In { expr, target } => {
            visitor.visit_expression(expr, ctx);
            match target {
                InTarget::Values(values) => {
                    for value in values {
                        visitor.visit_expression(value, ctx);
                    }
                }
                InTarget::SubQuery(query) => visitor.visit_plan(query, ctx),
            }
        }
        Predicate::Exists { query } => visitor.visit_plan(query, ctx),
        Predicate::Like { expr, .. } | Predicate::IsNull { expr } => {
            visitor.visit_expression(expr, ctx)
        }
    }
}

pub fn default_visit_projection<F: ExpressionVisit + ?Sized>(
    visitor: &mut F,
    proj: &Projection,
    ctx: &mut F::Ctx,
) {
    match proj {
        Projection::Expr { expression, .. } => visitor.visit_expression(expression, ctx),
        Projection::Column { .. } | Projection::Star { .. } => {}
    }
}

pub fn default_visit_plan<F: ExpressionVisit + ?Sized>(
    visitor: &mut F,
    plan: &QueryPlan,
    ctx: &mut F::Ctx,
) {
    for proj in &plan.select {
        visitor.visit_projection(proj, ctx);
    }
    if let Some(pred) = &plan.r#where {
        visitor.visit_predicate(pred, ctx);
    }
    if let Some(exprs) = &plan.group_by {
        for expr in exprs {
            visitor.visit_expression(expr, ctx);
        }
    }
    if let Some(pred) = &plan.having {
        visitor.visit_predicate(pred, ctx);
    }
    if let Some(terms) = &plan.order_by {
        for term in terms {
            visitor.visit_expression(&term.expr, ctx);
        }
    }
    if let Some(joins) = &plan.joins {
        for join in joins {
            visitor.visit_predicate(&join.on, ctx);
        }
    }
    if let Some(ctes) = &plan.ctes {
        for cte in ctes {
            visitor.visit_plan(&cte.query, ctx);
        }
    }
}
