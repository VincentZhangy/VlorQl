//! The [`PlanRewriter`] trait and the [`RewriterPipeline`] that chains
//! rewrite rules together.
//!
//! A rewriter takes a validated [`QueryPlan`] and returns a
//! semantically-equivalent plan that is cheaper to execute. Rules are
//! deliberately small and composable; [`RewriterPipeline`] applies a
//! sequence of them in order, threading each rule's output into the
//! next.

use crate::errors::VlorQLError;
use crate::schema::QueryPlan;
use std::fmt;

/// A logical rewrite rule over a [`QueryPlan`].
///
/// Implementations must preserve query semantics: the rewritten plan
/// must return the same rows as the input for every database state. A
/// rule that cannot safely rewrite part of a plan must leave that part
/// unchanged rather than guess.
pub trait PlanRewriter: fmt::Debug + Send + Sync {
    /// Returns a semantically-equivalent, ideally cheaper, plan.
    fn rewrite(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError>;
}

/// Applies an ordered list of [`PlanRewriter`]s, feeding each rule's
/// output into the next.
///
/// The pipeline owns its rules as boxed trait objects so heterogeneous
/// rewriters can be combined. Order matters: constant folding, for
/// example, is usually run before pushdown so folded literals can be
/// analyzed as constants.
///
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::{ConstantFolding, PlanRewriter, RewriterPipeline};
/// use vlorql_core::schema::{
///     BinaryOperator, DataType, Expression, FromClause, Projection, QueryPlan,
/// };
///
/// let plan = QueryPlan {
///     select: vec![Projection::Expr {
///         expression: Expression::BinaryOp {
///             left: Box::new(Expression::Literal { value: 20.into(), data_type: DataType::Int }),
///             op: BinaryOperator::Add,
///             right: Box::new(Expression::Literal { value: 5.into(), data_type: DataType::Int }),
///         },
///         alias: Some("total".to_owned()),
///     }],
///     from: FromClause { table: "t".to_owned(), alias: None },
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None, joins: None, ctes: None,
/// };
///
/// let pipeline = RewriterPipeline::new().with(ConstantFolding);
/// let rewritten = pipeline.rewrite(&plan).unwrap();
/// assert_eq!(
///     rewritten.select[0],
///     Projection::Expr {
///         expression: Expression::Literal { value: 25.into(), data_type: DataType::Int },
///         alias: Some("total".to_owned()),
///     },
/// );
/// ```
#[derive(Default, Debug)]
pub struct RewriterPipeline {
    rules: Vec<Box<dyn PlanRewriter>>,
}

impl RewriterPipeline {
    /// Creates an empty pipeline. An empty pipeline is the identity
    /// rewrite: it returns its input unchanged.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Appends a rule and returns the pipeline, for builder-style chaining.
    #[must_use]
    pub fn with(mut self, rule: impl PlanRewriter + 'static) -> Self {
        self.rules.push(Box::new(rule));
        self
    }

    /// Appends a rule in place.
    pub fn push(&mut self, rule: impl PlanRewriter + 'static) {
        self.rules.push(Box::new(rule));
    }

    /// Returns the number of rules in the pipeline.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Returns `true` when the pipeline has no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Applies every rule in order. If any rule fails, the error is
    /// propagated and no further rules run.
    pub fn rewrite(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        // First rule gets a clone of the input; subsequent rules own
        // the output of the previous rule so no extra clone is needed.
        let mut current = plan.clone();
        for rule in &self.rules {
            current = rule.rewrite(&current)?;
        }
        Ok(current)
    }

    /// Applies the pipeline repeatedly (up to `max_rounds` times) until the
    /// plan stops changing (fixed point).
    ///
    /// Constant folding may expose new pushdown opportunities, and pushdown
    /// may enable more column pruning. Running multiple rounds captures
    /// these cascading effects. In practice 2–3 rounds are sufficient; the
    /// default is 3.
    ///
    /// # Examples
    ///
    /// ```
    /// use vlorql_core::optimizer::{ConstantFolding, PlanRewriter, RewriterPipeline};
    /// use vlorql_core::schema::{
    ///     BinaryOperator, DataType, Expression, FromClause, Projection, QueryPlan,
    /// };
    ///
    /// let plan = QueryPlan {
    ///     select: vec![Projection::Expr {
    ///         expression: Expression::BinaryOp {
    ///             left: Box::new(Expression::Literal { value: 20.into(), data_type: DataType::Int }),
    ///             op: BinaryOperator::Add,
    ///             right: Box::new(Expression::Literal { value: 5.into(), data_type: DataType::Int }),
    ///         },
    ///         alias: Some("total".to_owned()),
    ///     }],
    ///     from: FromClause { table: "t".to_owned(), alias: None },
    ///     r#where: None, group_by: None, having: None,
    ///     order_by: None, limit: None, offset: None, joins: None, ctes: None,
    /// };
    ///
    /// let pipeline = RewriterPipeline::new().with(ConstantFolding);
    /// let rewritten = pipeline.repeat_until_stable(&plan, 3).unwrap();
    /// assert_eq!(
    ///     rewritten.select[0],
    ///     Projection::Expr {
    ///         expression: Expression::Literal { value: 25.into(), data_type: DataType::Int },
    ///         alias: Some("total".to_owned()),
    ///     },
    /// );
    /// ```
    pub fn repeat_until_stable(
        &self,
        plan: &QueryPlan,
        max_rounds: usize,
    ) -> Result<QueryPlan, VlorQLError> {
        let mut current = plan.clone();
        for _round in 0..max_rounds {
            let next = self.rewrite(&current)?;
            if next == current {
                return Ok(next);
            }
            current = next;
        }
        Ok(current)
    }
}
