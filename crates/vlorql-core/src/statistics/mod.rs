//! Table- and column-level statistics and pluggable providers.
//!
//! This module defines the data model for cost-estimation statistics
//! ([`ColumnStatistics`], [`TableStatistics`], [`StatisticsCatalog`]) and
//! the [`StatisticsProvider`] trait that loads them from an external
//! source. Two providers ship with the crate:
//! [`DummyStatisticsProvider`] (fixed in-memory catalog, for tests) and
//! [`ConfigFileStatisticsProvider`] (a JSON or YAML file on disk).
//!
//! Built on top of the providers, [`CardinalityEstimator`] turns those
//! statistics into result-row estimates and [`CostEstimator`] scores
//! plan operations with a three-axis [`Cost`] (CPU/IO/memory) so a
//! planner can compare alternative execution plans.

mod cardinality;
mod cost;
mod providers;
mod stats;

pub use cardinality::{
    CardinalityEstimator, DEFAULT_BETWEEN_SELECTIVITY, DEFAULT_JOIN_SELECTIVITY,
    DEFAULT_RANGE_SELECTIVITY, DEFAULT_SELECTIVITY, DEFAULT_TABLE_ROWS, LIKE_SELECTIVITY,
};
pub use cost::{
    Cost, CostEstimator, INDEX_SCAN_COST_PER_ROW, JOIN_CPU_COST_PER_PAIR, NETWORK_COST_PER_ROW,
    SORT_CPU_COST_PER_ROW, SORT_MEMORY_PER_ROW, TABLE_SCAN_COST_PER_ROW,
};
pub use providers::{
    ConfigFileStatisticsProvider, DummyStatisticsProvider, StatisticsProvider,
};
pub use stats::{ColumnStatistics, StatisticsCatalog, TableStatistics};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    fn sample_catalog() -> StatisticsCatalog {
        let mut users = TableStatistics {
            row_count: 10_000,
            last_analyzed: Some(
                "2026-07-17T12:00:00Z"
                    .parse()
                    .expect("timestamp should parse"),
            ),
            ..TableStatistics::default()
        };
        users.columns.insert(
            "id".to_owned(),
            ColumnStatistics {
                null_fraction: 0.0,
                distinct_count: 10_000,
                min_value: Some(json!(1)),
                max_value: Some(json!(10_000)),
                avg_column_width: Some(4),
                histogram: None,
            },
        );
        users.columns.insert(
            "email".to_owned(),
            ColumnStatistics {
                null_fraction: 0.05,
                distinct_count: 9_500,
                min_value: None,
                max_value: None,
                avg_column_width: Some(32),
                histogram: Some(vec![json!("a"), json!("m"), json!("z")]),
            },
        );

        let mut catalog = StatisticsCatalog::default();
        catalog.tables.insert("users".to_owned(), users);
        catalog
    }

    #[test]
    fn catalog_round_trips_through_json() {
        let catalog = sample_catalog();
        let value = to_value(&catalog).expect("catalog should serialize");
        let decoded: StatisticsCatalog =
            from_value(value).expect("catalog should deserialize");
        assert_eq!(decoded, catalog);
    }

    #[test]
    fn catalog_round_trips_through_yaml() {
        let catalog = sample_catalog();
        let yaml = serde_yaml::to_string(&catalog).expect("catalog should serialize to YAML");
        let decoded: StatisticsCatalog =
            serde_yaml::from_str(&yaml).expect("catalog should deserialize from YAML");
        assert_eq!(decoded, catalog);
    }

    #[test]
    fn column_statistics_defaults_are_neutral() {
        let stats = ColumnStatistics::default();
        assert_eq!(stats.null_fraction, 0.0);
        assert_eq!(stats.distinct_count, 0);
        assert!(stats.min_value.is_none());
        assert!(stats.max_value.is_none());
        assert!(stats.avg_column_width.is_none());
        assert!(stats.histogram.is_none());
    }

    #[test]
    fn missing_optional_fields_deserialize_to_defaults() {
        // A minimal record that only sets `row_count` must fill in the
        // remaining fields from their defaults.
        let value = json!({
            "tables": {
                "orders": { "row_count": 3 }
            }
        });
        let catalog: StatisticsCatalog =
            from_value(value).expect("partial record should deserialize");
        let table = catalog.table("orders").expect("orders should be present");
        assert_eq!(table.row_count, 3);
        assert!(table.columns.is_empty());
        assert!(table.last_analyzed.is_none());
    }

    #[test]
    fn catalog_accessors_locate_tables_and_columns() {
        let catalog = sample_catalog();
        assert_eq!(
            catalog.table("users").map(|table| table.row_count),
            Some(10_000)
        );
        assert_eq!(
            catalog
                .column("users", "email")
                .map(|column| column.distinct_count),
            Some(9_500)
        );
        assert!(catalog.table("missing").is_none());
        assert!(catalog.column("users", "missing").is_none());
    }

    #[tokio::test]
    async fn dummy_provider_serves_its_catalog() {
        let provider = DummyStatisticsProvider::new(sample_catalog());

        let table = provider
            .get_table_stats("users")
            .await
            .expect("lookup should succeed")
            .expect("users should be present");
        assert_eq!(table.row_count, 10_000);

        let column = provider
            .get_column_stats("users", "email")
            .await
            .expect("lookup should succeed")
            .expect("email should be present");
        assert_eq!(column.null_fraction, 0.05);

        assert!(provider
            .get_table_stats("missing")
            .await
            .expect("lookup should succeed")
            .is_none());
        assert!(provider
            .get_column_stats("users", "missing")
            .await
            .expect("lookup should succeed")
            .is_none());

        let catalog = provider
            .get_catalog_stats()
            .await
            .expect("catalog should be returned");
        assert_eq!(catalog, sample_catalog());
    }

    #[tokio::test]
    async fn dummy_provider_defaults_to_empty_catalog() {
        let provider = DummyStatisticsProvider::default();
        let catalog = provider
            .get_catalog_stats()
            .await
            .expect("catalog should be returned");
        assert!(catalog.tables.is_empty());
    }

    #[tokio::test]
    async fn config_file_provider_parses_json() {
        let contents = r#"{
            "tables": {
                "users": {
                    "row_count": 500,
                    "columns": {
                        "id": { "distinct_count": 500, "null_fraction": 0.0 }
                    }
                }
            }
        }"#;
        let provider = ConfigFileStatisticsProvider::from_str("stats.json", contents)
            .expect("JSON should parse");

        let table = provider
            .get_table_stats("users")
            .await
            .expect("lookup should succeed")
            .expect("users should be present");
        assert_eq!(table.row_count, 500);
        assert_eq!(
            provider
                .get_column_stats("users", "id")
                .await
                .expect("lookup should succeed")
                .map(|column| column.distinct_count),
            Some(500)
        );
    }

    #[tokio::test]
    async fn config_file_provider_parses_yaml() {
        let contents = "\
tables:
  users:
    row_count: 500
    columns:
      email:
        null_fraction: 0.1
        distinct_count: 480
";
        let provider = ConfigFileStatisticsProvider::from_str("stats.yaml", contents)
            .expect("YAML should parse");

        let column = provider
            .get_column_stats("users", "email")
            .await
            .expect("lookup should succeed")
            .expect("email should be present");
        assert_eq!(column.null_fraction, 0.1);
        assert_eq!(column.distinct_count, 480);
    }

    #[test]
    fn config_file_provider_reports_parse_errors() {
        let error = ConfigFileStatisticsProvider::from_str("stats.json", "not json")
            .expect_err("invalid JSON should fail");
        assert_eq!(error.error_code(), "G006");
    }

    #[test]
    fn config_file_provider_reports_missing_files() {
        let error = ConfigFileStatisticsProvider::load("/nonexistent/stats.json")
            .expect_err("missing file should fail");
        assert_eq!(error.error_code(), "G006");
    }
}
