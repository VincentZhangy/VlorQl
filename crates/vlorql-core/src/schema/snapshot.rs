//! Database schema snapshots and indexed lookup helpers.

use super::types::DataType;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Shared ownership type for schema snapshots used by validators and builders.
pub type ArcSchemaSnapshot = Arc<SchemaSnapshot>;

type TableIndex = HashMap<String, usize>;

fn new_table_index() -> Arc<OnceLock<TableIndex>> {
    Arc::new(OnceLock::new())
}

/// A database schema snapshot used for plan validation and prompt construction.
///
/// The snapshot owns the table list and lazily builds an internal
/// `HashMap<name, position>` for O(1) table lookups. The index is
/// `#[serde(skip)]`'d so it never appears in serialized output and
/// is rebuilt on demand after deserialization.
///
/// # Examples
///
/// ```
/// use vlorql_core::schema::{SchemaSnapshot, TableSchema, ColumnSchema, DataType, SchemaMetadata};
///
/// let snapshot = SchemaSnapshot::new(
///     vec![TableSchema {
///         name: "users".to_owned(),
///         columns: vec![ColumnSchema {
///             name: "id".to_owned(),
///             data_type: DataType::Int,
///             nullable: false,
///             description: None,
///             is_primary_key: true,
///             foreign_key: None,
///         }],
///         description: None,
///         primary_key: Some(vec!["id".to_owned()]),
///     }],
///     SchemaMetadata::default(),
/// );
/// assert_eq!(snapshot.table_count(), 1);
/// assert!(snapshot.get_table("users").is_some());
/// assert!(snapshot.get_column("users", "id").is_some());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemaSnapshot {
    /// The serializable representation remains a vector for stable API output.
    pub tables: Vec<TableSchema>,
    /// Free-form metadata describing the snapshot's source.
    #[serde(default)]
    pub metadata: SchemaMetadata,
    /// Runtime-only lookup index. It is rebuilt lazily after deserialization.
    #[serde(skip, default = "new_table_index")]
    #[schemars(skip)]
    table_index: Arc<OnceLock<TableIndex>>,
}

impl Default for SchemaSnapshot {
    fn default() -> Self {
        Self::new(Vec::new(), SchemaMetadata::default())
    }
}

impl PartialEq for SchemaSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.tables == other.tables && self.metadata == other.metadata
    }
}

impl Eq for SchemaSnapshot {}

impl SchemaSnapshot {
    /// Creates a snapshot and builds its table-name index immediately.
    pub fn new(tables: Vec<TableSchema>, metadata: SchemaMetadata) -> Self {
        let snapshot = Self {
            tables,
            metadata,
            table_index: new_table_index(),
        };
        snapshot.initialize_index();
        snapshot
    }

    /// Replaces the table list and rebuilds the runtime lookup index.
    pub fn set_tables(&mut self, tables: Vec<TableSchema>) {
        self.tables = tables;
        self.table_index = new_table_index();
        self.initialize_index();
    }

    /// Rebuilds the runtime lookup index after direct changes to `tables`.
    pub fn rebuild_index(&mut self) {
        self.table_index = new_table_index();
        self.initialize_index();
    }

    /// Looks up a table by its exact schema name.
    pub fn get_table(&self, name: &str) -> Option<&TableSchema> {
        let index = self
            .table_index
            .get_or_init(|| build_table_index(&self.tables));
        index
            .get(name)
            .and_then(|position| self.tables.get(*position))
    }

    /// Looks up a column by table and exact column name.
    pub fn get_column(&self, table: &str, column: &str) -> Option<&ColumnSchema> {
        self.get_table(table)?
            .columns
            .iter()
            .find(|candidate| candidate.name == column)
    }

    /// Returns the number of tables in this snapshot.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    fn initialize_index(&self) {
        let _ = self.table_index.set(build_table_index(&self.tables));
    }
}

fn build_table_index(tables: &[TableSchema]) -> TableIndex {
    let mut index = HashMap::with_capacity(tables.len());
    for (position, table) in tables.iter().enumerate() {
        index.insert(table.name.clone(), position);
    }
    index
}

/// Metadata describing where and when a schema snapshot was produced.
///
/// All fields are optional so a deserialized payload may omit any of
/// them; they exist to help operators audit the source of a snapshot
/// and are surfaced verbatim in the system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemaMetadata {
    /// Schema version string (e.g. `"v3"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Origin of the schema (e.g. the introspection job that produced it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// ISO-8601 timestamp at which the snapshot was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

/// A table and its columns in a schema snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableSchema {
    /// Table name as it appears in the database.
    pub name: String,
    /// The column definitions, in the order they should be exposed to the LLM.
    pub columns: Vec<ColumnSchema>,
    /// Optional human-readable description of the table's purpose.
    pub description: Option<String>,
    /// Optional list of column names that form the primary key.
    pub primary_key: Option<Vec<String>>,
}

/// A column and its database-level type and constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnSchema {
    /// Column name as it appears in the database.
    pub name: String,
    /// The SQL data type of the column.
    pub data_type: DataType,
    /// Whether the column allows `NULL` values.
    pub nullable: bool,
    /// Optional human-readable description of the column's purpose.
    pub description: Option<String>,
    /// Whether the column participates in the table's primary key.
    pub is_primary_key: bool,
    /// Optional foreign-key reference to another table and column.
    pub foreign_key: Option<ForeignKey>,
}

/// A foreign-key reference from one column to another table and column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    /// The referenced table name.
    pub foreign_table: String,
    /// The referenced column name.
    pub foreign_column: String,
}
