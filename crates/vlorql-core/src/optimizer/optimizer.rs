//! The [`QueryOptimizer`] façade that drives every optimisation pass.
//!
//! [`QueryOptimizer`] sits between validation and compilation. Given a
//! plan that has already passed schema, policy, operand, and dialect
//! checks, it applies a fixed sequence of semantics-preserving rewrites
//! and, when enabled, cost-based join reordering:
//!
//! 1. **Logical rewrites** (always in this order):
//!    [`ConstantFolding`], [`PredicatePushdown`], [`ColumnPruning`].
//!    Folding runs first so folded literals can be treated as constants
//!    by later passes. Pushdown and pruning are individually toggleable.
//! 2. **Cost-based join reordering** ([`JoinReorderer`]), applied by the
//!    async entry point when a statistics provider is configured and the
//!    join involves no more than [`Self::max_join_reorder_tables`]
//!    relations.
//!
//! Every pass is conservative: when it cannot prove a rewrite is safe it
//! leaves that part of the plan untouched, so the optimizer's output is
//! always semantically equivalent to its input. That equivalence is what
//! lets the pipeline re-run only the *policy* check after optimisation
//! rather than the full validation suite.

use std::sync::Arc;

use crate::errors::VlorQLError;
use crate::schema::QueryPlan;
use crate::statistics::{DummyStatisticsProvider, StatisticsProvider};

use super::{
    ColumnPruning, ConstantFolding, JoinReorderer, PlanRewriter, PredicatePushdown,
    RewriterPipeline, MAX_DP_RELATIONS,
};

/// Orchestrates all optimisation passes over a validated [`QueryPlan`].
///
/// Construct with [`QueryOptimizer::new`] to enable every pass (folding,
/// pushdown, pruning, and join reordering), then narrow the behaviour
/// with the `with_*` builder methods. Use [`QueryOptimizer::rewrites_only`]
/// for the logical rewrites without consulting statistics.
///
/// # Examples
///
/// ```
/// use vlorql_core::optimizer::QueryOptimizer;
/// use vlorql_core::statistics::DummyStatisticsProvider;
/// use std::sync::Arc;
///
/// let stats = Arc::new(DummyStatisticsProvider::default());
/// let optimizer = QueryOptimizer::new(stats)
///     .with_predicate_pushdown(true)
///     .with_join_reorder(false);
/// ```
#[derive(Clone)]
pub struct QueryOptimizer {
    /// Source of table/column statistics used by cost-based reordering.
    stats_provider: Arc<dyn StatisticsProvider>,
    /// Whether cost-based join reordering runs in [`Self::optimize_async`].
    enable_join_reorder: bool,
    /// Whether [`PredicatePushdown`] is part of the rewrite pipeline.
    enable_predicate_pushdown: bool,
    /// Whether [`ColumnPruning`] is part of the rewrite pipeline.
    enable_column_pruning: bool,
    /// The largest number of relations the optimizer will attempt to
    /// reorder. When a join chain has more relations than this, its
    /// order is left untouched so planning stays cheap. Defaults to
    /// [`MAX_DP_RELATIONS`]; within that bound the reorderer chooses
    /// exact dynamic programming, above it (when the cap is raised) the
    /// greedy heuristic.
    max_join_reorder_tables: usize,
}

impl QueryOptimizer {
    /// Creates an optimizer with every pass enabled, using `stats_provider`
    /// for cost-based join reordering.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        Self {
            stats_provider,
            enable_join_reorder: true,
            enable_predicate_pushdown: true,
            enable_column_pruning: true,
            max_join_reorder_tables: MAX_DP_RELATIONS,
        }
    }

    /// Creates an optimizer with only the logical rewrite rules (constant
    /// folding, predicate pushdown, column pruning) and no join
    /// reordering, so no real statistics source is required.
    pub fn rewrites_only() -> Self {
        Self {
            // Reordering is disabled, so this provider is never consulted.
            stats_provider: Arc::new(DummyStatisticsProvider::default()),
            enable_join_reorder: false,
            enable_predicate_pushdown: true,
            enable_column_pruning: true,
            max_join_reorder_tables: MAX_DP_RELATIONS,
        }
    }

    /// Enables or disables cost-based join reordering.
    #[must_use]
    pub fn with_join_reorder(mut self, enabled: bool) -> Self {
        self.enable_join_reorder = enabled;
        self
    }

    /// Enables or disables predicate pushdown.
    #[must_use]
    pub fn with_predicate_pushdown(mut self, enabled: bool) -> Self {
        self.enable_predicate_pushdown = enabled;
        self
    }

    /// Enables or disables column pruning.
    #[must_use]
    pub fn with_column_pruning(mut self, enabled: bool) -> Self {
        self.enable_column_pruning = enabled;
        self
    }

    /// Sets the maximum number of relations to attempt reordering over.
    /// Join chains larger than this keep their original order.
    #[must_use]
    pub fn with_max_join_reorder_tables(mut self, max: usize) -> Self {
        self.max_join_reorder_tables = max;
        self
    }

    /// Returns the relation cap above which join reordering is skipped.
    pub fn max_join_reorder_tables(&self) -> usize {
        self.max_join_reorder_tables
    }

    /// Builds the synchronous rewrite pipeline from the enabled flags.
    ///
    /// Constant folding is always first and always present; pushdown and
    /// pruning are appended only when enabled.
    fn rewrite_pipeline(&self) -> RewriterPipeline {
        let mut pipeline = RewriterPipeline::new().with(ConstantFolding);
        if self.enable_predicate_pushdown {
            pipeline.push(PredicatePushdown);
        }
        if self.enable_column_pruning {
            pipeline.push(ColumnPruning::new());
        }
        pipeline
    }

    /// Applies the synchronous logical rewrites (constant folding, and,
    /// when enabled, predicate pushdown and column pruning).
    ///
    /// This never consults statistics and never reorders joins; use
    /// [`Self::optimize_async`] for the full pipeline.
    pub fn optimize(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        self.rewrite_pipeline().rewrite(plan)
    }

    /// Applies the logical rewrites and then, when join reordering is
    /// enabled and the join is small enough, cost-based reordering.
    ///
    /// This is the primary entry point because reordering consults the
    /// statistics provider asynchronously. The returned plan is always
    /// semantically equivalent to `plan`.
    pub async fn optimize_async(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        let span = tracing::info_span!(
            "vlorql.optimize",
            join_reorder_enabled = self.enable_join_reorder,
        );
        let _enter = span.enter();
        let rewritten = self.optimize(plan)?;

        if self.enable_join_reorder && self.within_reorder_cap(&rewritten) {
            let reorderer = JoinReorderer::new(Arc::clone(&self.stats_provider));
            return reorderer.reorder(&rewritten).await;
        }

        Ok(rewritten)
    }

    /// Estimates the total cost of executing `plan`'s join chain in the
    /// order it is written, using the configured statistics provider.
    pub async fn estimated_cost(&self, plan: &QueryPlan) -> Result<f64, VlorQLError> {
        let reorderer = JoinReorderer::new(Arc::clone(&self.stats_provider));
        Ok(reorderer.estimate_plan_cost(plan).await?.total())
    }

    /// Returns `true` when `plan`'s relation count is within the reorder cap.
    fn within_reorder_cap(&self, plan: &QueryPlan) -> bool {
        let relation_count = 1 + plan.joins.as_ref().map_or(0, Vec::len);
        relation_count <= self.max_join_reorder_tables
    }
}

impl std::fmt::Debug for QueryOptimizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryOptimizer")
            .field("enable_join_reorder", &self.enable_join_reorder)
            .field("enable_predicate_pushdown", &self.enable_predicate_pushdown)
            .field("enable_column_pruning", &self.enable_column_pruning)
            .field("max_join_reorder_tables", &self.max_join_reorder_tables)
            .finish_non_exhaustive()
    }
}
