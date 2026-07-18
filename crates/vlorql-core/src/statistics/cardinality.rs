//! Cardinality (result-row) estimation driven by column statistics.
//!
//! A [`CardinalityEstimator`] answers three questions a cost-based
//! planner needs:
//!
//! * how many rows a base table holds
//!   ([`estimate_table_cardinality`](CardinalityEstimator::estimate_table_cardinality)),
//! * what fraction of a table survives a predicate
//!   ([`estimate_predicate_cardinality`](CardinalityEstimator::estimate_predicate_cardinality)), and
//! * how many rows a join produces
//!   ([`estimate_join_cardinality`](CardinalityEstimator::estimate_join_cardinality)).
//!
//! The estimates are deliberately simple textbook heuristics
//! (equality selectivity is `1 / distinct_count`, ranges are
//! interpolated over `min`/`max`, conjunctions multiply, disjunctions
//! use the inclusion–exclusion rule). They exist to *rank* alternative
//! plans, not to predict exact row counts, and every returned
//! selectivity is clamped to `0.0..=1.0`.
//!
//! Because [`StatisticsProvider`] is asynchronous, the estimation
//! methods are `async` as well.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::errors::VlorQLError;
use crate::schema::{ComparisonOperator, Expression, InTarget, Predicate};

use super::providers::StatisticsProvider;

/// Assumed row count for a table the provider has no statistics for.
pub const DEFAULT_TABLE_ROWS: u64 = 1_000;
/// Fallback selectivity when a column's statistics are unavailable.
pub const DEFAULT_SELECTIVITY: f64 = 0.1;
/// Fallback selectivity for a range comparison (`>`, `<`, `>=`, `<=`)
/// when `min`/`max` bounds are unknown.
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 1.0 / 3.0;
/// Fallback selectivity for a `BETWEEN` whose column has no `min`/`max`.
pub const DEFAULT_BETWEEN_SELECTIVITY: f64 = 0.1;
/// Conservative selectivity assumed for a `LIKE` pattern match.
pub const LIKE_SELECTIVITY: f64 = 0.05;
/// Fallback selectivity for a join predicate the estimator cannot resolve.
pub const DEFAULT_JOIN_SELECTIVITY: f64 = 0.1;

/// Estimates result cardinalities from a [`StatisticsProvider`].
///
/// Clone is cheap: the estimator only holds a shared `Arc` handle to
/// its provider, so it can be handed to as many planner tasks as
/// needed.
#[derive(Clone)]
pub struct CardinalityEstimator {
    stats_provider: Arc<dyn StatisticsProvider>,
}

impl fmt::Debug for CardinalityEstimator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CardinalityEstimator").finish_non_exhaustive()
    }
}

impl CardinalityEstimator {
    /// Creates an estimator backed by `stats_provider`.
    pub fn new(stats_provider: Arc<dyn StatisticsProvider>) -> Self {
        Self { stats_provider }
    }

    /// Returns the estimated number of rows in `table`.
    ///
    /// Falls back to [`DEFAULT_TABLE_ROWS`] when the provider has no
    /// statistics for the table, so downstream cost comparisons still
    /// have a non-zero value to work with.
    pub async fn estimate_table_cardinality(&self, table: &str) -> Result<u64, VlorQLError> {
        let stats = self.stats_provider.get_table_stats(table).await?;
        Ok(stats.map_or(DEFAULT_TABLE_ROWS, |table| table.row_count))
    }

    /// Estimates the selectivity of `pred` against `table`, i.e. the
    /// fraction of rows expected to satisfy it, in `0.0..=1.0`.
    ///
    /// Multiply the result by a table cardinality to obtain an expected
    /// row count.
    pub async fn estimate_predicate_cardinality(
        &self,
        table: &str,
        pred: &Predicate,
    ) -> Result<f64, VlorQLError> {
        self.predicate_selectivity(table, pred).await
    }

    /// Estimates the number of rows produced by joining `left_rows`
    /// with `right_rows` under `join_pred`.
    ///
    /// The result is `left_rows * right_rows * selectivity(join_pred)`.
    /// For an equi-join `left.col = right.col` the selectivity is
    /// `1 / max(distinct_left, distinct_right)`; other predicates fall
    /// back to [`DEFAULT_JOIN_SELECTIVITY`].
    pub async fn estimate_join_cardinality(
        &self,
        left_rows: u64,
        right_rows: u64,
        join_pred: &Predicate,
    ) -> Result<u64, VlorQLError> {
        let selectivity = self.join_selectivity(join_pred).await?;
        let estimate = left_rows as f64 * right_rows as f64 * selectivity;
        // Clamp into the representable `u64` range before rounding.
        Ok(estimate.round().clamp(0.0, u64::MAX as f64) as u64)
    }

    /// Recursively computes predicate selectivity. Boxed because the
    /// `And`/`Or`/`Not` arms recurse across an `await` point.
    fn predicate_selectivity<'a>(
        &'a self,
        table: &'a str,
        pred: &'a Predicate,
    ) -> Pin<Box<dyn Future<Output = Result<f64, VlorQLError>> + Send + 'a>> {
        Box::pin(async move {
            let selectivity = match pred {
                Predicate::Comparison { left, op, right } => {
                    self.comparison_selectivity(table, left, *op, right).await?
                }
                Predicate::And { left, right } => {
                    let l = self.predicate_selectivity(table, left).await?;
                    let r = self.predicate_selectivity(table, right).await?;
                    l * r
                }
                Predicate::Or { left, right } => {
                    let l = self.predicate_selectivity(table, left).await?;
                    let r = self.predicate_selectivity(table, right).await?;
                    l + r - l * r
                }
                Predicate::Not { child } => {
                    1.0 - self.predicate_selectivity(table, child).await?
                }
                Predicate::Between { expr, low, high } => {
                    self.between_selectivity(table, expr, low, high).await?
                }
                Predicate::In { expr, target } => {
                    let per_value = self.equality_selectivity(table, expr).await?;
                    match target {
                        InTarget::Values(values) => per_value * values.len() as f64,
                        InTarget::SubQuery(_) => per_value,
                    }
                }
                Predicate::Like { .. } => LIKE_SELECTIVITY,
                Predicate::IsNull { expr } => self.null_selectivity(table, expr).await?,
                Predicate::Exists { .. } => LIKE_SELECTIVITY, // EXISTS selectivity is approximated as LIKE
            };
            Ok(clamp01(selectivity))
        })
    }

    async fn comparison_selectivity(
        &self,
        table: &str,
        left: &Expression,
        op: ComparisonOperator,
        right: &Expression,
    ) -> Result<f64, VlorQLError> {
        let column = column_name(left).or_else(|| column_name(right));
        let distinct = match column {
            Some(column) => self.distinct_count(table, column).await?,
            None => None,
        };

        let selectivity = match op {
            // Equality: 1 / distinct_count when known.
            ComparisonOperator::Eq => match distinct {
                Some(distinct) => 1.0 / distinct as f64,
                None => DEFAULT_SELECTIVITY,
            },
            // Inequality: the complement of equality.
            ComparisonOperator::Neq => match distinct {
                Some(distinct) => 1.0 - 1.0 / distinct as f64,
                None => 1.0 - DEFAULT_SELECTIVITY,
            },
            // Range comparisons use a fixed heuristic fraction.
            ComparisonOperator::Gt
            | ComparisonOperator::Lt
            | ComparisonOperator::Gte
            | ComparisonOperator::Lte => DEFAULT_RANGE_SELECTIVITY,
            // Pattern matches are treated conservatively.
            ComparisonOperator::Like | ComparisonOperator::ILike => LIKE_SELECTIVITY,
            // `In`/`Between` operators only appear inside their dedicated
            // predicate variants; a bare comparison using them is unusual.
            ComparisonOperator::In | ComparisonOperator::Between => DEFAULT_SELECTIVITY,
        };
        Ok(selectivity)
    }

    async fn between_selectivity(
        &self,
        table: &str,
        expr: &Expression,
        low: &Expression,
        high: &Expression,
    ) -> Result<f64, VlorQLError> {
        let column = match column_name(expr) {
            Some(column) => column,
            None => return Ok(DEFAULT_BETWEEN_SELECTIVITY),
        };
        let stats = self.stats_provider.get_column_stats(table, column).await?;
        if let Some(stats) = stats {
            if let (Some(min), Some(max), Some(low), Some(high)) = (
                stats.min_value.as_ref().and_then(Value::as_f64),
                stats.max_value.as_ref().and_then(Value::as_f64),
                literal_f64(low),
                literal_f64(high),
            ) {
                if max > min {
                    return Ok(clamp01((high - low) / (max - min)));
                }
            }
        }
        Ok(DEFAULT_BETWEEN_SELECTIVITY)
    }

    async fn null_selectivity(
        &self,
        table: &str,
        expr: &Expression,
    ) -> Result<f64, VlorQLError> {
        let column = match column_name(expr) {
            Some(column) => column,
            None => return Ok(DEFAULT_SELECTIVITY),
        };
        let stats = self.stats_provider.get_column_stats(table, column).await?;
        Ok(stats.map_or(DEFAULT_SELECTIVITY, |stats| stats.null_fraction))
    }

    /// Per-value equality selectivity used by `IN` list expansion.
    async fn equality_selectivity(
        &self,
        table: &str,
        expr: &Expression,
    ) -> Result<f64, VlorQLError> {
        let column = match column_name(expr) {
            Some(column) => column,
            None => return Ok(DEFAULT_SELECTIVITY),
        };
        Ok(match self.distinct_count(table, column).await? {
            Some(distinct) => 1.0 / distinct as f64,
            None => DEFAULT_SELECTIVITY,
        })
    }

    fn join_selectivity<'a>(
        &'a self,
        pred: &'a Predicate,
    ) -> Pin<Box<dyn Future<Output = Result<f64, VlorQLError>> + Send + 'a>> {
        Box::pin(async move {
            match pred {
                Predicate::Comparison {
                    left,
                    op: ComparisonOperator::Eq,
                    right,
                } => self.equi_join_selectivity(left, right).await,
                // A conjunction of key equalities multiplies selectivities.
                Predicate::And { left, right } => {
                    let l = self.join_selectivity(left).await?;
                    let r = self.join_selectivity(right).await?;
                    Ok(clamp01(l * r))
                }
                _ => Ok(DEFAULT_JOIN_SELECTIVITY),
            }
        })
    }

    async fn equi_join_selectivity(
        &self,
        left: &Expression,
        right: &Expression,
    ) -> Result<f64, VlorQLError> {
        let (left, right) = match (qualified_column(left), qualified_column(right)) {
            (Some(left), Some(right)) => (left, right),
            // Without table qualifiers we cannot look up distinct counts.
            _ => return Ok(DEFAULT_JOIN_SELECTIVITY),
        };
        let left_distinct = self.distinct_count(left.0, left.1).await?;
        let right_distinct = self.distinct_count(right.0, right.1).await?;

        let distinct = match (left_distinct, right_distinct) {
            (Some(l), Some(r)) => l.max(r),
            (Some(l), None) => l,
            (None, Some(r)) => r,
            (None, None) => return Ok(DEFAULT_JOIN_SELECTIVITY),
        };
        Ok(1.0 / distinct.max(1) as f64)
    }

    /// Fetches a column's distinct count, treating `0` (the default for
    /// an unpopulated record) as "unknown".
    async fn distinct_count(
        &self,
        table: &str,
        column: &str,
    ) -> Result<Option<u64>, VlorQLError> {
        let stats = self.stats_provider.get_column_stats(table, column).await?;
        Ok(stats
            .map(|stats| stats.distinct_count)
            .filter(|count| *count > 0))
    }
}

/// Returns the column name if `expr` is a column reference.
fn column_name(expr: &Expression) -> Option<&str> {
    match expr {
        Expression::ColumnRef { column, .. } => Some(column.as_str()),
        _ => None,
    }
}

/// Returns `(table, column)` if `expr` is a *qualified* column reference.
fn qualified_column(expr: &Expression) -> Option<(&str, &str)> {
    match expr {
        Expression::ColumnRef {
            table: Some(table),
            column,
        } => Some((table.as_str(), column.as_str())),
        _ => None,
    }
}

/// Returns the numeric value of a literal expression, if it is numeric.
fn literal_f64(expr: &Expression) -> Option<f64> {
    match expr {
        Expression::Literal { value, .. } => value.as_f64(),
        _ => None,
    }
}

/// Clamps a selectivity into the valid `0.0..=1.0` range.
fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::DataType;
    use crate::statistics::{
        ColumnStatistics, DummyStatisticsProvider, StatisticsCatalog, TableStatistics,
    };
    use serde_json::json;

    fn estimator() -> CardinalityEstimator {
        let mut users = TableStatistics {
            row_count: 10_000,
            ..TableStatistics::default()
        };
        users.columns.insert(
            "id".to_owned(),
            ColumnStatistics {
                distinct_count: 10_000,
                min_value: Some(json!(1)),
                max_value: Some(json!(10_000)),
                ..ColumnStatistics::default()
            },
        );
        users.columns.insert(
            "status".to_owned(),
            ColumnStatistics {
                distinct_count: 4,
                null_fraction: 0.2,
                ..ColumnStatistics::default()
            },
        );

        let mut orders = TableStatistics {
            row_count: 50_000,
            ..TableStatistics::default()
        };
        orders.columns.insert(
            "user_id".to_owned(),
            ColumnStatistics {
                distinct_count: 8_000,
                ..ColumnStatistics::default()
            },
        );

        let mut catalog = StatisticsCatalog::default();
        catalog.tables.insert("users".to_owned(), users);
        catalog.tables.insert("orders".to_owned(), orders);
        CardinalityEstimator::new(Arc::new(DummyStatisticsProvider::new(catalog)))
    }

    fn column(table: &str, column: &str) -> Expression {
        Expression::ColumnRef {
            table: Some(table.to_owned()),
            column: column.to_owned(),
        }
    }

    fn literal(value: serde_json::Value, data_type: DataType) -> Expression {
        Expression::Literal { value, data_type }
    }

    fn eq(left: Expression, right: Expression) -> Predicate {
        Predicate::Comparison {
            left,
            op: ComparisonOperator::Eq,
            right,
        }
    }

    #[tokio::test]
    async fn table_cardinality_uses_row_count_or_default() {
        let estimator = estimator();
        assert_eq!(
            estimator.estimate_table_cardinality("users").await.unwrap(),
            10_000
        );
        assert_eq!(
            estimator
                .estimate_table_cardinality("unknown")
                .await
                .unwrap(),
            DEFAULT_TABLE_ROWS
        );
    }

    #[tokio::test]
    async fn equality_selectivity_is_reciprocal_of_distinct_count() {
        let estimator = estimator();
        let pred = eq(column("users", "status"), literal(json!("active"), DataType::String));
        let selectivity = estimator
            .estimate_predicate_cardinality("users", &pred)
            .await
            .unwrap();
        assert!((selectivity - 0.25).abs() < 1e-9);
        // Acceptance criterion: `col = value` selectivity is never > 1.0.
        assert!(selectivity <= 1.0);
    }

    #[tokio::test]
    async fn equality_selectivity_never_exceeds_one_without_stats() {
        let estimator = estimator();
        let pred = eq(column("users", "no_stats"), literal(json!(1), DataType::Int));
        let selectivity = estimator
            .estimate_predicate_cardinality("users", &pred)
            .await
            .unwrap();
        assert!((0.0..=1.0).contains(&selectivity));
    }

    #[tokio::test]
    async fn and_multiplies_or_uses_inclusion_exclusion() {
        let estimator = estimator();
        let left = eq(column("users", "status"), literal(json!("active"), DataType::String)); // 0.25
        let right = eq(column("users", "id"), literal(json!(1), DataType::Int)); // 1/10000

        let and = Predicate::And {
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        };
        let or = Predicate::Or {
            left: Box::new(left),
            right: Box::new(right),
        };

        let and_sel = estimator
            .estimate_predicate_cardinality("users", &and)
            .await
            .unwrap();
        let or_sel = estimator
            .estimate_predicate_cardinality("users", &or)
            .await
            .unwrap();

        let s1 = 0.25;
        let s2 = 1.0 / 10_000.0;
        assert!((and_sel - s1 * s2).abs() < 1e-9);
        assert!((or_sel - (s1 + s2 - s1 * s2)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn between_interpolates_over_min_max() {
        let estimator = estimator();
        // id in [2501, 5000] over a [1, 10000] range ~ 25%.
        let pred = Predicate::Between {
            expr: column("users", "id"),
            low: literal(json!(2501), DataType::Int),
            high: literal(json!(5000), DataType::Int),
        };
        let selectivity = estimator
            .estimate_predicate_cardinality("users", &pred)
            .await
            .unwrap();
        assert!((selectivity - 0.24_99).abs() < 0.01);
        assert!(selectivity <= 1.0);
    }

    #[tokio::test]
    async fn like_and_is_null_use_documented_values() {
        let estimator = estimator();
        let like = Predicate::Like {
            expr: column("users", "status"),
            pattern: "a%".to_owned(),
        };
        let is_null = Predicate::IsNull {
            expr: column("users", "status"),
        };
        assert!(
            (estimator
                .estimate_predicate_cardinality("users", &like)
                .await
                .unwrap()
                - LIKE_SELECTIVITY)
                .abs()
                < 1e-9
        );
        assert!(
            (estimator
                .estimate_predicate_cardinality("users", &is_null)
                .await
                .unwrap()
                - 0.2)
                .abs()
                < 1e-9
        );
    }

    #[tokio::test]
    async fn equi_join_uses_larger_distinct_count() {
        let estimator = estimator();
        // users.id (10000 distinct) = orders.user_id (8000 distinct)
        // selectivity = 1 / max(10000, 8000) = 1e-4.
        let pred = eq(column("users", "id"), column("orders", "user_id"));
        let cardinality = estimator
            .estimate_join_cardinality(10_000, 50_000, &pred)
            .await
            .unwrap();
        // 10000 * 50000 * (1/10000) = 50000.
        assert_eq!(cardinality, 50_000);
    }

    #[tokio::test]
    async fn join_without_qualifiers_falls_back_to_default() {
        let estimator = estimator();
        let pred = Predicate::Comparison {
            left: Expression::ColumnRef {
                table: None,
                column: "id".to_owned(),
            },
            op: ComparisonOperator::Eq,
            right: Expression::ColumnRef {
                table: None,
                column: "user_id".to_owned(),
            },
        };
        let cardinality = estimator
            .estimate_join_cardinality(100, 100, &pred)
            .await
            .unwrap();
        // 100 * 100 * 0.1 = 1000.
        assert_eq!(cardinality, 1_000);
    }
}
