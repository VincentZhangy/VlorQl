//! Logical rewrite rules applied to a validated [`QueryPlan`].
//!
//! Once a plan has passed validation and policy checks, the optimizer
//! applies semantics-preserving rewrites that reduce the work the SQL
//! backend has to do:
//!
//! * [`ConstantFolding`] statically evaluates constant sub-expressions
//!   (`20 + 5` becomes `25`).
//! * [`PredicatePushdown`] moves single-relation `WHERE` conjuncts down
//!   into the CTE they filter, so rows are discarded earlier.
//! * [`ColumnPruning`] removes CTE output columns that no consumer reads.
//!
//! Every rule implements [`PlanRewriter`]; [`RewriterPipeline`] chains
//! them in a chosen order.
//!
//! Cost-based [`JoinReorderer`] is a separate, `async` optimizer (it
//! consults a statistics provider) rather than a [`PlanRewriter`]: it
//! reorders an inner-join chain to minimize estimated cost.
//!
//! # Model limitations
//!
//! The plan model's [`FromClause`](crate::schema::FromClause) is a bare
//! table name — there is no inline-subquery relation. The only nestable
//! relation is a [`CommonTableExpression`](crate::schema::CommonTableExpression).
//! Pushdown and pruning therefore operate on **CTEs**, not on synthetic
//! FROM subqueries. Every rule is conservative: when it cannot prove a
//! rewrite is safe it leaves that part of the plan untouched, so the
//! output is always semantically equivalent to the input.

pub mod analyze;
mod fold;
mod join_reorder;
mod prune;
mod pushdown;
mod rules;
pub mod visitor;

pub use fold::ConstantFolding;
pub use join_reorder::{JoinGraph, JoinReorderer, MAX_DP_RELATIONS};
pub use prune::ColumnPruning;
pub use pushdown::PredicatePushdown;
pub use rules::{PlanRewriter, RewriterPipeline};

use crate::errors::VlorQLError;
use crate::schema::QueryPlan;
use crate::statistics::Cost;
use crate::statistics::StatisticsProvider;
use std::sync::Arc;

/// [`QueryPlan`](crate::schema::QueryPlan).
///
/// The optimizer applies a fixed sequence of logical rewrites (constant
/// folding, predicate pushdown, column pruning) and, when a statistics
/// provider is available, cost-based join reordering.
///
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::QueryOptimizer;
/// use vlorql_core::statistics::DummyStatisticsProvider;
/// use std::sync::Arc;
///
/// let stats = Arc::new(DummyStatisticsProvider::default());
/// let optimizer = QueryOptimizer::new(stats);
/// ```
#[derive(Debug, Clone)]
pub struct QueryOptimizer {
    /// The synchronous rewrite pipeline (folding, pushdown, pruning).
    pipeline: Arc<RewriterPipeline>,
    /// Optional async join reorderer, available when statistics are present.
    join_reorderer: Option<JoinReorderer>,
    /// Flag to enable/disable join reordering at runtime.
    enable_join_reorder: bool,
}

impl QueryOptimizer {
    /// Creates a new optimizer with all rewrite rules enabled.
    ///
    /// When `stats_provider` is a non-empty provider, join reordering is
    /// also enabled. Pass `DummyStatisticsProvider::default()` to skip
    /// join reordering.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        let join_reorderer = Some(JoinReorderer::new(Arc::clone(&stats_provider)));
        Self {
            pipeline: Arc::new(
                RewriterPipeline::new()
                    .with(ConstantFolding)
                    .with(PredicatePushdown)
                    .with(ColumnPruning::new()),
            ),
            join_reorderer,
            enable_join_reorder: true,
        }
    }

    /// Creates an optimizer with only the logical rewrite rules (no join
    /// reordering), regardless of whether statistics are available.
    pub fn rewrites_only() -> Self {
        Self {
            pipeline: Arc::new(
                RewriterPipeline::new()
                    .with(ConstantFolding)
                    .with(PredicatePushdown)
                    .with(ColumnPruning::new()),
            ),
            join_reorderer: None,
            enable_join_reorder: false,
        }
    }

    /// Enables or disables join reordering at runtime.
    #[must_use]
    pub fn with_join_reorder(mut self, enabled: bool) -> Self {
        self.enable_join_reorder = enabled;
        self
    }

    /// Returns the estimated cost of executing `plan`'s join chain.
    ///
    /// Returns `None` when no join reorderer is configured.
    pub async fn estimated_cost(&self, plan: &QueryPlan) -> Option<Cost> {
        match self.join_reorderer {
            Some(ref jr) => Some(jr.estimate_plan_cost(plan).await.unwrap_or_default()),
            None => None,
        }
    }

    /// Applies synchronous rewrite rules (constant folding, predicate
    /// pushdown, column pruning) to the plan.
    pub fn optimize(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        self.pipeline.rewrite(plan)
    }

    /// Applies the rewrite pipeline in fixed-point iteration (up to
    /// `max_rounds`) until the plan stabilizes. See
    /// [`RewriterPipeline::repeat_until_stable`].
    pub fn optimize_repeat(
        &self,
        plan: &QueryPlan,
        max_rounds: usize,
    ) -> Result<QueryPlan, VlorQLError> {
        self.pipeline.repeat_until_stable(plan, max_rounds)
    }

    /// Applies all rewrite rules **and**, if enabled, cost-based join
    /// reordering. This is the async entry point because join reordering
    /// consults the statistics provider.
    ///
    /// The synchronous rewrite pipeline is offloaded to a blocking thread
    /// via [`tokio::task::spawn_blocking`] so it does not block the Tokio
    /// worker threads.
    pub async fn optimize_async(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        // Offload the CPU-intensive rewrite pipeline to a blocking thread.
        let this = self.clone();
        let plan_clone = plan.clone();
        let plan = tokio::task::spawn_blocking(move || this.pipeline.rewrite(&plan_clone))
            .await
            .map_err(|join_err| {
                VlorQLError::config(
                    crate::errors::ConfigErrorKind::InternalError {
                        reason: format!("optimizer spawn_blocking join failed: {join_err}"),
                    },
                    serde_json::json!({"operation": "optimize_async"}),
                )
            })??;
        if self.enable_join_reorder
            && let Some(ref reorderer) = self.join_reorderer
        {
            return reorderer.reorder(&plan).await;
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests;
