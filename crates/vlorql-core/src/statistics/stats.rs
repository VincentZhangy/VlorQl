//! Table- and column-level statistics used for cost estimation.
//!
//! These structures describe the shape of the data behind a schema:
//! how many rows a table holds, how many distinct values a column has,
//! what fraction of a column is null, and so on. A query planner or
//! optimizer consumes them to estimate selectivity and cardinality.
//!
//! The types are deliberately serialization-friendly (`serde` +
//! `serde_json::Value` for open-ended bounds) so a
//! [`StatisticsProvider`](crate::statistics::StatisticsProvider) can
//! load them from a database system table, a JSON/YAML file, or any
//! other external source.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Statistics describing a single column.
///
/// Every field defaults to a neutral value so a partially populated
/// record (e.g. a provider that only knows `null_fraction`) is still
/// valid. `min_value`, `max_value`, and `histogram` use
/// [`serde_json::Value`] because a column can hold any SQL type.
///
/// # Examples
///
/// ```
/// use vlorql_core::statistics::ColumnStatistics;
/// use serde_json::json;
///
/// let stats = ColumnStatistics {
///     null_fraction: 0.1,
///     distinct_count: 1_000,
///     min_value: Some(json!(1)),
///     max_value: Some(json!(9_999)),
///     avg_column_width: Some(4),
///     histogram: None,
/// };
/// assert_eq!(stats.distinct_count, 1_000);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColumnStatistics {
    /// Fraction of rows whose value is `NULL`, in the range `0.0..=1.0`.
    pub null_fraction: f64,
    /// Estimated number of distinct (non-null) values in the column.
    pub distinct_count: u64,
    /// Smallest observed value, if known.
    pub min_value: Option<serde_json::Value>,
    /// Largest observed value, if known.
    pub max_value: Option<serde_json::Value>,
    /// Average width of a stored value in bytes, if known.
    pub avg_column_width: Option<u32>,
    /// Optional equi-depth histogram bucket boundaries.
    pub histogram: Option<Vec<serde_json::Value>>,
}

/// Statistics describing a single table and its columns.
///
/// # Examples
///
/// ```
/// use vlorql_core::statistics::{ColumnStatistics, TableStatistics};
///
/// let mut table = TableStatistics {
///     row_count: 42,
///     ..TableStatistics::default()
/// };
/// table
///     .columns
///     .insert("id".to_owned(), ColumnStatistics::default());
/// assert_eq!(table.row_count, 42);
/// assert!(table.columns.contains_key("id"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TableStatistics {
    /// Estimated number of rows in the table.
    pub row_count: u64,
    /// Per-column statistics, keyed by column name.
    pub columns: HashMap<String, ColumnStatistics>,
    /// When the statistics were last collected, if known.
    pub last_analyzed: Option<chrono::DateTime<chrono::Utc>>,
}

/// A catalog of table statistics for an entire schema.
///
/// This is the top-level document a
/// [`StatisticsProvider`](crate::statistics::StatisticsProvider)
/// produces and the shape a JSON/YAML statistics file is expected to
/// have.
///
/// # Examples
///
/// ```
/// use vlorql_core::statistics::{StatisticsCatalog, TableStatistics};
///
/// let mut catalog = StatisticsCatalog::default();
/// catalog
///     .tables
///     .insert("users".to_owned(), TableStatistics::default());
/// assert!(catalog.tables.contains_key("users"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatisticsCatalog {
    /// Per-table statistics, keyed by table name.
    pub tables: HashMap<String, TableStatistics>,
}

impl StatisticsCatalog {
    /// Returns the statistics for `table_name`, if present.
    pub fn table(&self, table_name: &str) -> Option<&TableStatistics> {
        self.tables.get(table_name)
    }

    /// Returns the statistics for `table_name.column_name`, if present.
    pub fn column(&self, table_name: &str, column_name: &str) -> Option<&ColumnStatistics> {
        self.tables
            .get(table_name)
            .and_then(|table| table.columns.get(column_name))
    }
}
