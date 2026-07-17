//! Pluggable sources of table and column statistics.
//!
//! A [`StatisticsProvider`] abstracts *where* statistics come from so
//! the planner does not care whether they were collected from a live
//! database, read from a checked-in file, or synthesized for a test.
//!
//! Two implementations ship with the crate:
//!
//! * [`DummyStatisticsProvider`] serves a fixed in-memory
//!   [`StatisticsCatalog`] and is convenient for tests.
//! * [`ConfigFileStatisticsProvider`] loads a catalog from a JSON or
//!   YAML file on disk.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::errors::{ConfigErrorKind, VlorQLError};

use super::stats::{ColumnStatistics, StatisticsCatalog, TableStatistics};

/// A source of table- and column-level statistics.
///
/// Implementations are `Send + Sync` so a single provider can be shared
/// across the async tasks that build and cost query plans.
#[async_trait]
pub trait StatisticsProvider: Send + Sync {
    /// Returns statistics for `table_name`, or `None` if the provider
    /// has none for that table.
    async fn get_table_stats(
        &self,
        table_name: &str,
    ) -> Result<Option<TableStatistics>, VlorQLError>;

    /// Returns statistics for `table_name.column_name`, or `None` if the
    /// provider has none for that column.
    async fn get_column_stats(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<Option<ColumnStatistics>, VlorQLError>;

    /// Returns the full statistics catalog known to the provider.
    async fn get_catalog_stats(&self) -> Result<StatisticsCatalog, VlorQLError>;
}

/// A [`StatisticsProvider`] backed by a fixed in-memory catalog.
///
/// Useful in tests and for supplying hand-authored estimates. Construct
/// it with a prepared [`StatisticsCatalog`] via [`Self::new`], or with
/// an empty catalog via [`Default`].
///
/// # Examples
///
/// ```
/// # #[tokio::main]
/// # async fn main() {
/// use vlorql_core::statistics::{
///     DummyStatisticsProvider, StatisticsCatalog, StatisticsProvider, TableStatistics,
/// };
///
/// let mut catalog = StatisticsCatalog::default();
/// catalog.tables.insert(
///     "users".to_owned(),
///     TableStatistics {
///         row_count: 1_000,
///         ..TableStatistics::default()
///     },
/// );
///
/// let provider = DummyStatisticsProvider::new(catalog);
/// let stats = provider.get_table_stats("users").await.unwrap().unwrap();
/// assert_eq!(stats.row_count, 1_000);
/// assert!(provider.get_table_stats("missing").await.unwrap().is_none());
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct DummyStatisticsProvider {
    catalog: StatisticsCatalog,
}

impl DummyStatisticsProvider {
    /// Creates a provider that serves the given catalog.
    pub fn new(catalog: StatisticsCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl StatisticsProvider for DummyStatisticsProvider {
    async fn get_table_stats(
        &self,
        table_name: &str,
    ) -> Result<Option<TableStatistics>, VlorQLError> {
        Ok(self.catalog.table(table_name).cloned())
    }

    async fn get_column_stats(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<Option<ColumnStatistics>, VlorQLError> {
        Ok(self.catalog.column(table_name, column_name).cloned())
    }

    async fn get_catalog_stats(&self) -> Result<StatisticsCatalog, VlorQLError> {
        Ok(self.catalog.clone())
    }
}

/// The on-disk encoding of a statistics file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFormat {
    Json,
    Yaml,
}

/// A [`StatisticsProvider`] that loads a catalog from a JSON or YAML file.
///
/// The file format is inferred from the extension: `.yaml`/`.yml` are
/// parsed as YAML and anything else is parsed as JSON. The file is read
/// eagerly when the provider is constructed so later lookups are
/// infallible reads of the in-memory catalog.
///
/// # Examples
///
/// ```no_run
/// # #[tokio::main]
/// # async fn main() {
/// use vlorql_core::statistics::{ConfigFileStatisticsProvider, StatisticsProvider};
///
/// let provider = ConfigFileStatisticsProvider::load("stats.yaml").unwrap();
/// let catalog = provider.get_catalog_stats().await.unwrap();
/// println!("{} tables", catalog.tables.len());
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ConfigFileStatisticsProvider {
    path: PathBuf,
    catalog: StatisticsCatalog,
}

impl ConfigFileStatisticsProvider {
    /// Reads and parses a statistics file, inferring the format from the
    /// file extension (`.yaml`/`.yml` for YAML, otherwise JSON).
    ///
    /// Returns a [`ConfigErrorKind::ConfigFileError`] if the file cannot
    /// be read or its contents cannot be parsed.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
        let path = path.as_ref().to_path_buf();
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            Self::file_error(&path, format!("could not read file: {error}"))
        })?;
        Self::from_str(path, &contents)
    }

    /// Parses a catalog from an in-memory string, using `path` only to
    /// choose the format and to build error messages.
    ///
    /// This is the I/O-free core of [`Self::load`] and is handy in tests
    /// that do not want to touch the filesystem.
    pub fn from_str(path: impl AsRef<Path>, contents: &str) -> Result<Self, VlorQLError> {
        let path = path.as_ref().to_path_buf();
        let catalog = match Self::format_of(&path) {
            FileFormat::Yaml => serde_yaml::from_str(contents)
                .map_err(|error| Self::file_error(&path, format!("invalid YAML: {error}")))?,
            FileFormat::Json => serde_json::from_str(contents)
                .map_err(|error| Self::file_error(&path, format!("invalid JSON: {error}")))?,
        };
        Ok(Self { path, catalog })
    }

    /// Returns the path the catalog was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn format_of(path: &Path) -> FileFormat {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("yaml" | "yml") => FileFormat::Yaml,
            _ => FileFormat::Json,
        }
    }

    fn file_error(path: &Path, reason: String) -> VlorQLError {
        VlorQLError::config(
            ConfigErrorKind::ConfigFileError {
                path: path.display().to_string(),
                reason,
            },
            serde_json::json!({ "path": path.display().to_string() }),
        )
    }
}

#[async_trait]
impl StatisticsProvider for ConfigFileStatisticsProvider {
    async fn get_table_stats(
        &self,
        table_name: &str,
    ) -> Result<Option<TableStatistics>, VlorQLError> {
        Ok(self.catalog.table(table_name).cloned())
    }

    async fn get_column_stats(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<Option<ColumnStatistics>, VlorQLError> {
        Ok(self.catalog.column(table_name, column_name).cloned())
    }

    async fn get_catalog_stats(&self) -> Result<StatisticsCatalog, VlorQLError> {
        Ok(self.catalog.clone())
    }
}
