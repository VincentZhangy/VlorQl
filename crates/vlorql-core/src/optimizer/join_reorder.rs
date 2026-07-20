//! Cost-based join reordering for multi-table [`QueryPlan`]s.
//!
//! A plan's `from` clause plus its `joins` list describe a **left-deep**
//! join chain: `from ⋈ j₀ ⋈ j₁ ⋈ …`. When every join is an `INNER`
//! join, that chain is semantically identical to the cross product of
//! all its relations filtered by the conjunction of every `ON`
//! predicate (and the outer `WHERE`). That equivalence is what makes
//! reordering safe: the individual `ON` conjuncts can be redistributed
//! across a *different* left-deep order without changing the result, as
//! long as each conjunct lands on a join step by which all the relations
//! it references have been introduced.
//!
//! This module turns a plan into a [`JoinGraph`] (relations are nodes,
//! two-table `ON` conjuncts are edges), searches for a cheaper order
//! with [`JoinReorderer`], and writes the winning order back into the
//! plan. Two search strategies are provided:
//!
//! * **Dynamic programming** (Selinger-style, left-deep) for small
//!   problems (`≤ `[`MAX_DP_RELATIONS`]) — it enumerates connected
//!   subsets and keeps the cheapest plan for each, so it finds the
//!   optimal left-deep order.
//! * **Greedy** for larger problems — it seeds from the smallest
//!   relation and repeatedly appends the relation that yields the
//!   cheapest next join.
//!
//! # Safety and degradation
//!
//! The reorderer never changes query semantics. It declines to reorder
//! (returning the input plan unchanged) whenever it cannot prove a
//! rewrite is safe:
//!
//! * any join is not `INNER` (outer and cross joins are order-sensitive),
//! * the two-table join graph is disconnected (a genuine cross product),
//! * two relations share an effective name (an unaliased self-join),
//! * or the cheapest order found is not cheaper than the original.
//!
//! Non-equi join conditions (`a.x > b.y`) are still reorderable — they
//! remain edges in the graph — they simply fall back to the estimator's
//! [`DEFAULT_JOIN_SELECTIVITY`](crate::statistics::DEFAULT_JOIN_SELECTIVITY)
//! when their selectivity cannot be derived from column statistics.

use std::collections::HashMap;
use std::sync::Arc;

use crate::errors::VlorQLError;
use crate::schema::{FromClause, JoinClause, JoinType, Predicate, QueryPlan};
use crate::statistics::{Cost, CostEstimator, DEFAULT_JOIN_SELECTIVITY, StatisticsProvider};

use super::analyze::{columns_in_predicate, combine_conjuncts, split_conjuncts};

/// The largest relation count for which [`JoinReorderer`] uses exact
/// dynamic programming; above this it falls back to the greedy heuristic.
///
/// The DP search is exponential in the number of relations (it visits
/// every connected subset), so it is only used for small joins.
pub const MAX_DP_RELATIONS: usize = 5;

/// A single relation participating in a join (a base table plus its
/// optional alias).
#[derive(Debug, Clone)]
struct Relation {
    /// The `FROM`/`JOIN` clause exactly as it should be re-emitted.
    from: FromClause,
    /// The name used to match column qualifiers against this relation:
    /// the alias when present, otherwise the bare table name.
    key: String,
}

impl Relation {
    fn new(from: FromClause) -> Self {
        let key = from.alias.clone().unwrap_or_else(|| from.table.clone());
        Self { from, key }
    }
}

/// One top-level `AND` conjunct extracted from the join `ON` clauses,
/// annotated with the relations it references.
#[derive(Debug, Clone)]
struct Conjunct {
    /// The predicate itself, re-emitted verbatim when rewriting.
    pred: Predicate,
    /// Bitmask of relation indices (into [`JoinGraph::relations`]) that this
    /// conjunct references through a qualified column. Bit `i` is set when
    /// relation `i` is referenced. Using a `u32` bitmask instead of a
    /// `HashSet` makes [`JoinGraph::connecting`] a pure bitwise operation.
    tables: u32,
    /// `true` when the conjunct has a column that could not be attributed
    /// to a known relation (an unqualified reference, or a qualifier that
    /// matches no relation key). Such a conjunct is placed on the final
    /// join step, where every relation is guaranteed to be available.
    ambiguous: bool,
}

/// A join relationship graph: relations are nodes and two-table `ON`
/// conjuncts are edges.
///
/// Build one from a plan with [`JoinGraph::build`]. The graph only
/// exists for plans that are safe to reorder; [`JoinGraph::build`]
/// returns `None` for everything else (see the [module docs](super)).
#[derive(Debug, Clone)]
pub struct JoinGraph {
    /// The relations to be joined, in the plan's original order
    /// (`relations[0]` is the `from` clause).
    relations: Vec<Relation>,
    /// Every `ON` conjunct across all joins, flattened.
    conjuncts: Vec<Conjunct>,
}

impl JoinGraph {
    /// Builds a join graph from `plan`, or returns `None` when the plan
    /// is not safe to reorder.
    ///
    /// `None` is returned when the plan has no joins, any join is not
    /// `INNER`, two relations share an effective name, or the two-table
    /// join graph is disconnected. In every one of those cases the caller
    /// should leave the plan untouched.
    pub fn build(plan: &QueryPlan) -> Option<Self> {
        let joins = plan.joins.as_ref()?;
        if joins.is_empty() {
            return None;
        }
        // Only inner joins can be freely reordered.
        if joins.iter().any(|join| join.join_type != JoinType::Inner) {
            return None;
        }

        // Node 0 is the `from` relation; the rest come from the joins.
        let mut relations = Vec::with_capacity(joins.len() + 1);
        relations.push(Relation::new(plan.from.clone()));
        for join in joins {
            relations.push(Relation::new(join.right_table.clone()));
        }
        let n = relations.len();

        // Effective names must be unique, otherwise a column qualifier
        // cannot be attributed to a single relation.
        let mut key_index: HashMap<String, usize> = HashMap::with_capacity(n);
        for (index, relation) in relations.iter().enumerate() {
            if key_index.insert(relation.key.clone(), index).is_some() {
                return None;
            }
        }

        // Flatten every join predicate into annotated conjuncts.
        let mut conjuncts = Vec::new();
        for join in joins {
            for pred in split_conjuncts(&join.on) {
                let mut tables: u32 = 0;
                let mut ambiguous = false;
                for (qualifier, _column) in columns_in_predicate(&pred) {
                    match qualifier.and_then(|q| key_index.get(&q).copied()) {
                        Some(index) => {
                            tables |= 1u32 << index;
                        }
                        None => ambiguous = true,
                    }
                }
                conjuncts.push(Conjunct {
                    pred,
                    tables,
                    ambiguous,
                });
            }
        }

        // The two-table conjuncts form the edge set; the graph must be
        // connected for a left-deep order to exist without a cross join.
        if !Self::is_connected(n, &conjuncts) {
            return None;
        }

        Some(Self {
            relations,
            conjuncts,
        })
    }

    /// The number of relations (nodes) in the graph.
    pub fn relation_count(&self) -> usize {
        self.relations.len()
    }

    /// Union-find connectivity check over the two-table conjunct edges.
    fn is_connected(n: usize, conjuncts: &[Conjunct]) -> bool {
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }

        for conjunct in conjuncts {
            if conjunct.ambiguous || conjunct.tables.count_ones() != 2 {
                continue;
            }
            let bits = conjunct.tables;
            let a = bits.trailing_zeros() as usize;
            let b = (bits & !(1u32 << a)).trailing_zeros() as usize;
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            parent[ra] = rb;
        }

        let root = find(&mut parent, 0);
        (1..n).all(|i| find(&mut parent, i) == root)
    }

    /// Returns the conjunct indices that connect `j` to the relations
    /// already in `in_set_mask` — i.e. conjuncts fully covered by
    /// `in_set_mask ∪ {j}` that reference `j` (either as a join edge to the
    /// set, or as a single-table filter on `j`).
    fn connecting(&self, in_set_mask: u32, j: usize) -> Vec<usize> {
        let j_bit = 1u32 << j;
        let needed = in_set_mask | j_bit;
        self.conjuncts
            .iter()
            .enumerate()
            .filter(|(_, conjunct)| {
                if conjunct.ambiguous || conjunct.tables & j_bit == 0 {
                    return false;
                }
                // Every referenced relation must already be available.
                let remaining = conjunct.tables & !needed;
                if remaining != 0 {
                    return false;
                }
                // Either a single-table filter on `j`, or it touches the
                // existing set (a genuine join edge).
                conjunct.tables == j_bit || (conjunct.tables & in_set_mask) != 0
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Combines the given conjuncts into a single `AND` predicate.
    fn combine(&self, indices: &[usize]) -> Option<Predicate> {
        combine_conjuncts(
            indices
                .iter()
                .map(|&i| self.conjuncts[i].pred.clone())
                .collect(),
        )
    }

    /// Assigns every conjunct to a step of the final left-deep `order`.
    ///
    /// Returns a vector of length `order.len()`; entry `k` holds the
    /// conjunct indices that belong on the `ON` clause of the join that
    /// introduces `order[k]`. Entry `0` (the `from` relation) is always
    /// empty. A conjunct is placed on the step of its latest-introduced
    /// relation (floored to step 1); ambiguous conjuncts go on the last
    /// step, where every relation is available.
    fn assign_conjuncts(&self, order: &[usize]) -> Vec<Vec<usize>> {
        let n = order.len();
        // position[relation_index] = its slot in `order`.
        let mut position = vec![0usize; n];
        for (slot, &relation) in order.iter().enumerate() {
            position[relation] = slot;
        }

        let mut steps = vec![Vec::new(); n];
        for (index, conjunct) in self.conjuncts.iter().enumerate() {
            let step = if conjunct.ambiguous || conjunct.tables == 0 {
                n - 1
            } else if conjunct.tables.count_ones() == 1 {
                // Single-table filter: apply at the step that introduces
                // the table. For the seed (step 0) this means the first
                // join step (step 1), since there is no ON clause on the
                // `from` relation itself.
                let t = conjunct.tables.trailing_zeros() as usize;
                position[t].max(1)
            } else {
                // Multi-table conjunct: place on the latest-introduced
                // relation's step.
                (0..n)
                    .filter(|&i| conjunct.tables & (1u32 << i) != 0)
                    .map(|t| position[t])
                    .max()
                    .expect("non-empty table set")
            };
            steps[step].push(index);
        }
        steps
    }
}

/// A cost-based join reorderer.
///
/// Holds a [`CostEstimator`] (a cheap `Arc` handle to a statistics
/// provider) and rewrites a [`QueryPlan`]'s join order to minimize
/// estimated cost. See the [module docs](super) for the algorithm and the
/// safety guarantees.
#[derive(Debug, Clone)]
pub struct JoinReorderer {
    cost: CostEstimator,
}

/// A dynamic-programming table entry: the cheapest known left-deep plan
/// for a given subset of relations.
#[derive(Debug, Clone)]
struct DpEntry {
    /// Total cost of producing this subset.
    cost: Cost,
    /// Estimated cardinality of this subset's join result.
    card: u64,
    /// The relation order that achieves `cost`.
    order: Vec<usize>,
}

impl JoinReorderer {
    /// Creates a reorderer backed by `stats_provider`.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        Self {
            cost: CostEstimator::new(stats_provider),
        }
    }

    /// Creates a reorderer that reuses an existing [`CostEstimator`].
    pub fn with_cost_estimator(cost: CostEstimator) -> Self {
        Self { cost }
    }

    /// Returns a plan whose joins are reordered for minimal estimated
    /// cost, or a clone of `plan` unchanged when reordering is unsafe or
    /// would not reduce cost.
    ///
    /// This is the primary entry point. It builds a [`JoinGraph`], runs
    /// the appropriate search (DP for `≤ `[`MAX_DP_RELATIONS`] relations,
    /// greedy otherwise), and only rewrites the plan when the new order
    /// is both different and cheaper than the original.
    pub async fn reorder(&self, plan: &QueryPlan) -> Result<QueryPlan, VlorQLError> {
        let Some(graph) = JoinGraph::build(plan) else {
            return Ok(plan.clone());
        };
        let n = graph.relation_count();
        if n < 2 {
            return Ok(plan.clone());
        }

        let (base_card, scan_cost) = self.base_estimates(&graph).await?;

        let order = if n <= MAX_DP_RELATIONS {
            self.dp_order(&graph, &base_card, &scan_cost).await?
        } else {
            self.greedy_order(&graph, &base_card, &scan_cost).await?
        };

        let identity: Vec<usize> = (0..n).collect();
        if order == identity {
            return Ok(plan.clone());
        }

        // Never make a plan worse: only rewrite when the new order is
        // strictly cheaper than the original.
        let (new_cost, _) = self
            .cost_of_order(&graph, &order, &base_card, &scan_cost)
            .await?;
        let (old_cost, _) = self
            .cost_of_order(&graph, &identity, &base_card, &scan_cost)
            .await?;
        if new_cost.total() >= old_cost.total() {
            return Ok(plan.clone());
        }

        Ok(self.rewrite_plan(plan, &graph, &order))
    }

    /// Estimates the cost of executing `plan`'s join chain in the order
    /// it is written.
    ///
    /// For a plan with no reorderable join graph this is just the cost of
    /// scanning the `from` relation. Useful for asserting that a
    /// reordered plan is cheaper than the original.
    pub async fn estimate_plan_cost(&self, plan: &QueryPlan) -> Result<Cost, VlorQLError> {
        match JoinGraph::build(plan) {
            Some(graph) => {
                let (base_card, scan_cost) = self.base_estimates(&graph).await?;
                let identity: Vec<usize> = (0..graph.relation_count()).collect();
                let (cost, _) = self
                    .cost_of_order(&graph, &identity, &base_card, &scan_cost)
                    .await?;
                Ok(cost)
            }
            None => self.cost.estimate_scan(&plan.from.table, None).await,
        }
    }

    /// Precomputes each relation's base cardinality and scan cost.
    async fn base_estimates(
        &self,
        graph: &JoinGraph,
    ) -> Result<(Vec<u64>, Vec<Cost>), VlorQLError> {
        let n = graph.relation_count();
        let mut base_card = Vec::with_capacity(n);
        let mut scan_cost = Vec::with_capacity(n);
        for relation in &graph.relations {
            base_card.push(
                self.cost
                    .cardinality()
                    .estimate_table_cardinality(&relation.from.table)
                    .await?,
            );
            scan_cost.push(self.cost.estimate_scan(&relation.from.table, None).await?);
        }
        Ok((base_card, scan_cost))
    }

    /// Greedy join ordering: seed from the smallest relation, then
    /// repeatedly append the connected relation with the cheapest join.
    async fn greedy_order(
        &self,
        graph: &JoinGraph,
        base_card: &[u64],
        scan_cost: &[Cost],
    ) -> Result<Vec<usize>, VlorQLError> {
        let n = graph.relation_count();

        // Seed table: smallest base cardinality, ties broken by index.
        // `reorder()` guards n >= 2 before calling greedy_order, so the
        // range iterator always yields at least one element.
        let seed = (0..n)
            .min_by(|&a, &b| base_card[a].cmp(&base_card[b]).then(a.cmp(&b)))
            .unwrap_or(0);

        let mut in_set: u32 = 1u32 << seed;
        let mut order = vec![seed];
        let mut running_cost = scan_cost[seed];
        let mut running_card = base_card[seed];

        while order.len() < n {
            let mut best: Option<(f64, usize, Cost, u64)> = None;
            // Prefer relations connected to the current set; only fall
            // back to unconnected candidates if none are connected.
            for connected_only in [true, false] {
                for j in 0..n {
                    if in_set & (1u32 << j) != 0 {
                        continue;
                    }
                    let connections = graph.connecting(in_set, j);
                    if connected_only && connections.is_empty() {
                        continue;
                    }
                    let card = self
                        .step_cardinality(graph, in_set, running_card, base_card[j], j)
                        .await?;
                    let candidate = self.cost.estimate_join(running_cost, scan_cost[j], card);
                    let score = candidate.total();
                    if best.is_none_or(|(best_score, ..)| score < best_score) {
                        best = Some((score, j, candidate, card));
                    }
                }
                if best.is_some() {
                    break;
                }
            }

            let Some((_, j, cost, card)) = best else {
                // No candidate found — append remaining relations and stop.
                order.extend((0..n).filter(|&j| in_set & (1u32 << j) == 0));
                break;
            };
            in_set |= 1u32 << j;
            order.push(j);
            running_cost = cost;
            running_card = card;
        }

        Ok(order)
    }

    /// Exact left-deep dynamic programming: keep the cheapest plan for
    /// each connected subset of relations, growing subsets one relation
    /// at a time.
    async fn dp_order(
        &self,
        graph: &JoinGraph,
        base_card: &[u64],
        scan_cost: &[Cost],
    ) -> Result<Vec<usize>, VlorQLError> {
        let n = graph.relation_count();
        let full: u32 = (1u32 << n) - 1;

        let mut dp: HashMap<u32, DpEntry> = HashMap::new();
        for i in 0..n {
            dp.insert(
                1u32 << i,
                DpEntry {
                    cost: scan_cost[i],
                    card: base_card[i],
                    order: vec![i],
                },
            );
        }

        // Visit subsets in increasing size so every source subset is
        // finalized before it is extended.
        let mut masks: Vec<u32> = (1..=full).collect();
        masks.sort_by_key(|mask| mask.count_ones());

        for mask in masks {
            let Some(entry) = dp.get(&mask).cloned() else {
                continue;
            };

            for j in 0..n {
                if mask & (1u32 << j) != 0 {
                    continue;
                }
                let connections = graph.connecting(mask, j);
                if connections.is_empty() {
                    continue; // keep growth connected
                }
                let card = self
                    .step_cardinality(graph, mask, entry.card, base_card[j], j)
                    .await?;
                let cost = self.cost.estimate_join(entry.cost, scan_cost[j], card);
                let new_mask = mask | (1u32 << j);

                // Prefer a cheaper plan; break exact ties toward the order
                // seeded from the smaller base table. The tie-break matters
                // because this cost model is symmetric — a left-deep chain's
                // cost collapses to (first-step cardinality + final
                // cardinality), so the two endpoints of the cheapest join
                // edge cost the same. Seeding from the smaller relation keeps
                // the output deterministic and matches the greedy heuristic.
                let seed = entry.order[0];
                let better = dp.get(&new_mask).is_none_or(|existing| {
                    let cheaper = cost.total() < existing.cost.total();
                    let tied = cost.total() == existing.cost.total();
                    cheaper || (tied && base_card[seed] < base_card[existing.order[0]])
                });
                if better {
                    let mut new_order = entry.order.clone();
                    new_order.push(j);
                    dp.insert(
                        new_mask,
                        DpEntry {
                            cost,
                            card,
                            order: new_order,
                        },
                    );
                }
            }
        }

        Ok(dp
            .get(&full)
            .map(|entry| entry.order.clone())
            .unwrap_or_else(|| (0..n).collect()))
    }

    /// Cost of executing `order` as a left-deep chain, mirroring the
    /// per-step estimation the search uses.
    async fn cost_of_order(
        &self,
        graph: &JoinGraph,
        order: &[usize],
        base_card: &[u64],
        scan_cost: &[Cost],
    ) -> Result<(Cost, u64), VlorQLError> {
        let seed = order[0];
        let mut in_set: u32 = 1u32 << seed;
        let mut cost = scan_cost[seed];
        let mut card = base_card[seed];

        for &j in &order[1..] {
            let step_card = self
                .step_cardinality(graph, in_set, card, base_card[j], j)
                .await?;
            cost = self.cost.estimate_join(cost, scan_cost[j], step_card);
            card = step_card;
            in_set |= 1u32 << j;
        }
        Ok((cost, card))
    }

    /// Estimates the cardinality of joining relation `j` onto the set in
    /// `in_set_mask`, using the conjuncts that connect them (or a default
    /// selectivity when none apply).
    async fn step_cardinality(
        &self,
        graph: &JoinGraph,
        in_set_mask: u32,
        running_card: u64,
        base_j: u64,
        j: usize,
    ) -> Result<u64, VlorQLError> {
        let connections = graph.connecting(in_set_mask, j);
        match graph.combine(&connections) {
            Some(pred) => {
                self.cost
                    .cardinality()
                    .estimate_join_cardinality(running_card, base_j, &pred)
                    .await
            }
            None => {
                // No connecting predicate: fall back to a default-selectivity
                // product (a near-cross-join estimate).
                let estimate = running_card as f64 * base_j as f64 * DEFAULT_JOIN_SELECTIVITY;
                Ok(estimate.round().clamp(0.0, u64::MAX as f64) as u64)
            }
        }
    }

    /// Rebuilds `plan` with its relations in `order`, redistributing the
    /// `ON` conjuncts across the new join steps.
    fn rewrite_plan(&self, plan: &QueryPlan, graph: &JoinGraph, order: &[usize]) -> QueryPlan {
        let steps = graph.assign_conjuncts(order);

        let mut rewritten = plan.clone();
        rewritten.from = graph.relations[order[0]].from.clone();

        let mut joins = Vec::with_capacity(order.len() - 1);
        for (slot, &relation) in order.iter().enumerate().skip(1) {
            let on = graph.combine(&steps[slot]).unwrap_or_else(true_predicate);
            joins.push(JoinClause {
                join_type: JoinType::Inner,
                right_table: graph.relations[relation].from.clone(),
                on,
            });
        }
        rewritten.joins = Some(joins);
        rewritten
    }
}

/// A trivially-true predicate (`1 = 1`), used only as a safety net if a
/// join step somehow has no connecting conjunct. For a connected graph
/// this is never reached, because each new relation contributes its
/// linking edge to the step that introduces it.
fn true_predicate() -> Predicate {
    use crate::schema::{ComparisonOperator, DataType, Expression};
    let one = || Expression::Literal {
        value: serde_json::json!(1),
        data_type: DataType::Int,
    };
    Predicate::Comparison {
        left: one(),
        op: ComparisonOperator::Eq,
        right: one(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        ComparisonOperator, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
        QueryPlan,
    };
    use crate::statistics::{
        ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
    };
    use std::collections::HashSet;

    // --- builders -------------------------------------------------------

    fn from(table: &str) -> FromClause {
        FromClause {
            table: table.to_owned(),
            alias: None,
        }
    }

    fn col(table: &str, column: &str) -> Expression {
        Expression::ColumnRef {
            table: Some(table.to_owned()),
            column: column.to_owned(),
        }
    }

    fn cmp(left: Expression, op: ComparisonOperator, right: Expression) -> Predicate {
        Predicate::Comparison { left, op, right }
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

    /// Collects every `ON` conjunct in a plan as a sorted multiset of
    /// serialized predicates, so a rewrite can be checked for predicate
    /// preservation. `Predicate` is not `Ord`, so its JSON encoding is
    /// used as the comparison key.
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
        names.insert(plan.from.table.clone());
        if let Some(joins) = &plan.joins {
            for join in joins {
                names.insert(join.right_table.table.clone());
            }
        }
        names
    }

    // --- 3-table reordering --------------------------------------------

    #[tokio::test]
    async fn three_table_join_starts_from_smallest_base_table() {
        // Chain: users(10k) — orders(50k) — items(100), written
        // largest-first. The reorderer should seed from `items` (the
        // smallest) and lower the estimated cost.
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

        // Base table is now the smallest relation.
        assert_eq!(reordered.from.table, "items");

        // Same relations, same predicates, still two inner joins.
        assert_eq!(relation_names(&reordered), relation_names(&plan));
        assert_eq!(on_conjuncts(&plan), on_conjuncts(&reordered));
        let joins = reordered.joins.as_ref().unwrap();
        assert_eq!(joins.len(), 2);
        assert!(joins.iter().all(|j| j.join_type == JoinType::Inner));

        // And the reorder actually reduced the estimated cost.
        let reordered_cost = reorderer.estimate_plan_cost(&reordered).await.unwrap();
        assert!(
            reordered_cost.total() < original_cost.total(),
            "expected {reordered_cost:?} < {original_cost:?}"
        );
    }

    // --- non-equi (complex) join conditions ----------------------------

    #[tokio::test]
    async fn non_equi_join_condition_degrades_to_default_selectivity() {
        // Linear chain a(100) — b(50k) — c(10k): `a.x = b.x` is an equi
        // edge, `b.y > c.y` is a non-equi edge. The plan is written
        // starting from `c`, so its first join is the expensive non-equi
        // `c ⋈ b`. Reordering must still succeed — the non-equi edge falls
        // back to a default join selectivity rather than being rejected —
        // and it should reseed from `a` so the cheap equi join runs first.
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

        // No predicate is lost or duplicated, even the non-equi one.
        assert_eq!(on_conjuncts(&plan), on_conjuncts(&reordered));
        assert_eq!(relation_names(&reordered), relation_names(&plan));

        // The cheap equi join now runs first: `a` seeds the chain instead
        // of the original `c`.
        assert_eq!(reordered.from.table, "a");

        // And the rewrite is a strict improvement.
        let original_cost = reorderer.estimate_plan_cost(&plan).await.unwrap();
        let reordered_cost = reorderer.estimate_plan_cost(&reordered).await.unwrap();
        assert!(
            reordered_cost.total() < original_cost.total(),
            "expected {reordered_cost:?} < {original_cost:?}"
        );
    }

    // --- missing statistics --------------------------------------------

    #[tokio::test]
    async fn missing_statistics_fall_back_to_default_cardinality() {
        // An empty catalog: every table falls back to DEFAULT_TABLE_ROWS,
        // so all orders cost the same and the plan is left unchanged
        // rather than churned pointlessly.
        let plan = plan_with_joins(
            "users",
            vec![
                inner_join("orders", eq(col("users", "id"), col("orders", "user_id"))),
                inner_join("items", eq(col("orders", "id"), col("items", "order_id"))),
            ],
        );

        let reorderer = reorderer(StatisticsCatalog::default());
        let reordered = reorderer.reorder(&plan).await.unwrap();

        // No statistics, no cost difference -> no change, and no panic.
        assert_eq!(reordered, plan);
    }

    // --- safety: outer joins are not reordered -------------------------

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
        // orders is not joined to anything (a cross product) -> bail.
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
}
