//! A three-dimensional cost model for comparing execution plans.
//!
//! Every operation is scored on three axes — [`Cost::cpu`],
//! [`Cost::io`], and [`Cost::memory`] — so a planner can weigh, say, a
//! cheap-IO/expensive-CPU plan against its opposite. Costs compose with
//! `+` (run two operations) and `*` by a scalar (repeat an operation),
//! and they order by their [scalar total](Cost::total) so
//! `min`/`max`/`sort` pick the cheapest plan.
//!
//! [`CostEstimator`] turns cardinality estimates into concrete costs
//! for the three plan building blocks the initial planner needs: table
//! scans, joins, and sorts. It shares the [`CardinalityEstimator`]'s
//! [`StatisticsProvider`](crate::statistics::StatisticsProvider), so a
//! scan's cost reflects the same row estimates used elsewhere.

use std::ops::{Add, Mul};
use std::sync::Arc;

use crate::errors::VlorQLError;
use crate::schema::Predicate;

use super::cardinality::CardinalityEstimator;
use super::providers::StatisticsProvider;

/// CPU cost charged per row emitted by a sequential table scan.
pub const TABLE_SCAN_COST_PER_ROW: f64 = 1.0;
/// CPU cost charged per row emitted by an index scan.
pub const INDEX_SCAN_COST_PER_ROW: f64 = 0.1;
/// CPU cost charged per candidate row pair examined by a join.
pub const JOIN_CPU_COST_PER_PAIR: f64 = 0.01;
/// Cost charged per row shipped across the network.
pub const NETWORK_COST_PER_ROW: f64 = 0.5;
/// Multiplier applied to the `n log n` comparison term of a sort.
pub const SORT_CPU_COST_PER_ROW: f64 = 1.0;
/// Memory charged to buffer one row while sorting.
pub const SORT_MEMORY_PER_ROW: f64 = 1.0;

/// The estimated cost of an operation, split across three resources.
///
/// The fields are additive resource *quantities*, not a single score.
/// [`Cost::total`] collapses them into one comparable number, and the
/// [`PartialOrd`]/[`Ord`] impls order by that total so a collection of
/// candidate costs can be sorted directly.
///
/// # Examples
///
/// ```
/// use vlorql_core::statistics::Cost;
///
/// let scan = Cost { cpu: 100.0, io: 10.0, memory: 0.0 };
/// let sort = Cost { cpu: 20.0, io: 0.0, memory: 50.0 };
/// let combined = scan + sort;
/// assert_eq!(combined.cpu, 120.0);
/// assert!(scan.total() > sort.total());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Estimated CPU work (in abstract per-row units).
    pub cpu: f64,
    /// Estimated IO work (rows read from or written to storage).
    pub io: f64,
    /// Estimated peak memory (rows held in memory at once).
    pub memory: f64,
}

impl Cost {
    /// A zero cost, the additive identity.
    pub const ZERO: Self = Self {
        cpu: 0.0,
        io: 0.0,
        memory: 0.0,
    };

    /// Creates a cost from its three components.
    pub fn new(cpu: f64, io: f64, memory: f64) -> Self {
        Self { cpu, io, memory }
    }

    /// Collapses the three resources into a single comparable scalar.
    ///
    /// This is a plain sum of the components; callers that want to
    /// weight the axes differently can read the fields directly.
    pub fn total(&self) -> f64 {
        self.cpu + self.io + self.memory
    }
}

impl Add for Cost {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            cpu: self.cpu + rhs.cpu,
            io: self.io + rhs.io,
            memory: self.memory + rhs.memory,
        }
    }
}

impl Mul<f64> for Cost {
    type Output = Self;

    /// Scales every resource by `rhs` (e.g. to repeat an operation).
    fn mul(self, rhs: f64) -> Self {
        Self {
            cpu: self.cpu * rhs,
            io: self.io * rhs,
            memory: self.memory * rhs,
        }
    }
}

impl PartialOrd for Cost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.total().partial_cmp(&other.total())
    }
}

/// Turns cardinality estimates into concrete [`Cost`] values.
///
/// Clone is cheap: the estimator only holds a [`CardinalityEstimator`],
/// which itself is a shared `Arc` handle to a
/// [`StatisticsProvider`](crate::statistics::StatisticsProvider).
#[derive(Debug, Clone)]
pub struct CostEstimator {
    cardinality: CardinalityEstimator,
}

impl CostEstimator {
    /// Creates a cost estimator backed by `stats_provider`.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        Self {
            cardinality: CardinalityEstimator::new(stats_provider),
        }
    }

    /// Creates a cost estimator that shares an existing
    /// [`CardinalityEstimator`].
    pub fn with_cardinality(cardinality: CardinalityEstimator) -> Self {
        Self { cardinality }
    }

    /// Returns the underlying cardinality estimator.
    pub fn cardinality(&self) -> &CardinalityEstimator {
        &self.cardinality
    }

    /// Estimates the cost of scanning `table`, optionally reduced by a
    /// pushed-down `filter`.
    ///
    /// IO is charged for every stored row (the scan must read them all);
    /// CPU is charged only for the rows that survive `filter`, using the
    /// sequential-scan per-row weight. The output row count is folded
    /// into `memory` so downstream operators can see the scan's fan-out.
    pub async fn estimate_scan(
        &self,
        table: &str,
        filter: Option<&Predicate>,
    ) -> Result<Cost, VlorQLError> {
        let rows = self.cardinality.estimate_table_cardinality(table).await?;
        let selectivity = match filter {
            Some(pred) => {
                self.cardinality
                    .estimate_predicate_cardinality(table, pred)
                    .await?
            }
            None => 1.0,
        };
        let output_rows = rows as f64 * selectivity;
        Ok(Cost {
            cpu: output_rows * TABLE_SCAN_COST_PER_ROW,
            io: rows as f64,
            memory: output_rows,
        })
    }

    /// Estimates the cost of joining two already-costed inputs that
    /// together produce `join_cardinality` rows.
    ///
    /// The child costs are summed (both inputs must be produced), then a
    /// nested-loop-style CPU term is charged for the candidate pairs the
    /// join examines, approximated here by the output cardinality. The
    /// result set is buffered in `memory`.
    pub fn estimate_join(
        &self,
        left_cost: Cost,
        right_cost: Cost,
        join_cardinality: u64,
    ) -> Cost {
        let pairs = join_cardinality as f64;
        let join_cost = Cost {
            cpu: pairs * JOIN_CPU_COST_PER_PAIR,
            io: 0.0,
            memory: pairs,
        };
        left_cost + right_cost + join_cost
    }

    /// Estimates the cost of sorting `cardinality` rows.
    ///
    /// CPU follows the `n log2 n` comparison count of a comparison sort
    /// (with a floor of `n` so a single row still costs something), and
    /// every row is buffered in `memory`.
    pub fn estimate_sort(&self, cardinality: u64) -> Cost {
        let n = cardinality as f64;
        let comparisons = if cardinality > 1 {
            n * n.log2()
        } else {
            n
        };
        Cost {
            cpu: comparisons * SORT_CPU_COST_PER_ROW,
            io: 0.0,
            memory: n * SORT_MEMORY_PER_ROW,
        }
    }

    /// Estimates the network cost of shipping `cardinality` rows to the
    /// caller.
    pub fn estimate_network(&self, cardinality: u64) -> Cost {
        Cost {
            cpu: cardinality as f64 * NETWORK_COST_PER_ROW,
            io: 0.0,
            memory: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComparisonOperator, DataType, Expression, Predicate};
    use crate::statistics::{
        ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
    };
    use serde_json::json;

    fn estimator() -> CostEstimator {
        let mut users = TableStatistics {
            row_count: 10_000,
            ..TableStatistics::default()
        };
        users.columns.insert(
            "status".to_owned(),
            ColumnStatistics {
                distinct_count: 4,
                ..ColumnStatistics::default()
            },
        );

        let mut catalog = StatisticsCatalog::default();
        catalog.tables.insert("users".to_owned(), users);
        CostEstimator::new(Arc::new(DummyStatisticsProvider::new(catalog)))
    }

    #[test]
    fn cost_add_is_componentwise() {
        let a = Cost::new(1.0, 2.0, 3.0);
        let b = Cost::new(10.0, 20.0, 30.0);
        assert_eq!(a + b, Cost::new(11.0, 22.0, 33.0));
    }

    #[test]
    fn cost_mul_scales_each_component() {
        let scaled = Cost::new(1.0, 2.0, 3.0) * 2.0;
        assert_eq!(scaled, Cost::new(2.0, 4.0, 6.0));
    }

    #[test]
    fn cost_default_and_zero_are_empty() {
        assert_eq!(Cost::default(), Cost::ZERO);
        assert_eq!(Cost::default().total(), 0.0);
    }

    #[test]
    fn cost_orders_by_total() {
        let cheap = Cost::new(1.0, 1.0, 1.0);
        let pricey = Cost::new(10.0, 0.0, 0.0);
        assert!(cheap < pricey);

        let mut costs = [pricey, cheap];
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(costs[0], cheap);
    }

    #[tokio::test]
    async fn scan_without_filter_charges_every_row() {
        let cost = estimator().estimate_scan("users", None).await.unwrap();
        assert_eq!(cost.io, 10_000.0);
        assert_eq!(cost.cpu, 10_000.0);
        assert_eq!(cost.memory, 10_000.0);
    }

    #[tokio::test]
    async fn scan_with_filter_reads_all_rows_but_emits_fewer() {
        // status = 'active' has selectivity 1/4, so 2500 rows survive.
        let filter = Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "status".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::Literal {
                value: json!("active"),
                data_type: DataType::String,
            },
        };
        let cost = estimator()
            .estimate_scan("users", Some(&filter))
            .await
            .unwrap();
        // IO still reads the whole table.
        assert_eq!(cost.io, 10_000.0);
        // CPU/memory reflect the filtered output.
        assert_eq!(cost.cpu, 2_500.0);
        assert_eq!(cost.memory, 2_500.0);
    }

    #[test]
    fn join_sums_children_and_adds_pair_cost() {
        let est = estimator();
        let left = Cost::new(100.0, 200.0, 50.0);
        let right = Cost::new(10.0, 20.0, 5.0);
        let joined = est.estimate_join(left, right, 1_000);
        assert_eq!(joined.cpu, 100.0 + 10.0 + 1_000.0 * JOIN_CPU_COST_PER_PAIR);
        assert_eq!(joined.io, 220.0);
        assert_eq!(joined.memory, 50.0 + 5.0 + 1_000.0);
    }

    #[test]
    fn sort_cost_grows_super_linearly() {
        let est = estimator();
        let small = est.estimate_sort(10);
        let large = est.estimate_sort(1_000);
        // n log2 n: 10 * ~3.32 vs 1000 * ~9.97.
        assert!((small.cpu - 10.0 * 10.0_f64.log2()).abs() < 1e-6);
        assert!((large.cpu - 1_000.0 * 1_000.0_f64.log2()).abs() < 1e-6);
        assert!(large.cpu / large.memory > small.cpu / small.memory);
    }

    #[test]
    fn sort_of_single_row_is_linear_floor() {
        let est = estimator();
        let one = est.estimate_sort(1);
        assert_eq!(one.cpu, 1.0);
        assert_eq!(one.memory, 1.0);
    }

    #[test]
    fn network_cost_scales_with_rows() {
        let est = estimator();
        let cost = est.estimate_network(200);
        assert_eq!(cost.cpu, 200.0 * NETWORK_COST_PER_ROW);
    }
}
