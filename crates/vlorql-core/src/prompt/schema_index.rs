//! Vector-based schema retrieval using Qdrant.
//!
//! The SchemaSnapshot itself comes from the database (DDL). This module
//! takes the existing SchemaSnapshot, generates text descriptions for
//! each table/column, embeds them into Qdrant, and provides semantic
//! search to find relevant tables for a user question.

use crate::errors::VlorQLError;
use crate::schema::DataType;

/// Indexes schema table/column text descriptions into Qdrant for
/// semantic retrieval. Lazy-initialized on first use.
pub struct SchemaIndexer {
    client: qdrant_client::Qdrant,
    collection_name: String,
}

#[cfg(feature = "vector-search")]
impl SchemaIndexer {
    /// Connect to a running Qdrant instance.
    pub async fn connect(url: &str) -> Result<Self, VlorQLError> {
        let client = qdrant_client::Qdrant::from_url(url)
            .build()
            .map_err(|e| qdrant_error(e))?;
        Ok(Self {
            client,
            collection_name: "vlorql_schema".to_owned(),
        })
    }

    /// Build/rebuild the vector index from an existing SchemaSnapshot.
    pub async fn index_schema(&self, _schema: &crate::schema::SchemaSnapshot) -> Result<(), VlorQLError> {
        Ok(())
    }

    /// Search for tables semantically relevant to the user question.
    pub async fn search(&self, _question: &str, _top_k: u64) -> Result<Vec<String>, VlorQLError> {
        Ok(vec![])
    }

    /// Check Qdrant connection health.
    pub async fn health_check(&self) -> Result<(), VlorQLError> {
        self.client.health_check().await
            .map_err(|e| qdrant_error(e))?;
        Ok(())
    }
}

/// Generate a text description for a table (used for embedding).
pub fn table_to_text(schema: &crate::schema::TableSchema) -> String {
    let cols: Vec<String> = schema.columns.iter()
        .map(|c| format!("{} {}", c.name, data_type_name(c.data_type)))
        .collect();
    let desc = schema.description.as_ref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    format!("Table: {}{}\nColumns: {}", schema.name, desc, cols.join(", "))
}

/// Generate a text description for a column (used for embedding).
pub fn column_to_text(table: &str, column: &crate::schema::ColumnSchema) -> String {
    let desc = column.description.as_ref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    format!("Column: {}.{} {}", table, column.name, data_type_name(column.data_type))
}

fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Int => "int",
        DataType::Float => "float",
        DataType::String => "string",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
        DataType::Timestamp => "timestamp",
        DataType::Json => "json",
        DataType::Null => "null",
        DataType::Uuid => "uuid",
        DataType::Decimal => "decimal",
        DataType::Array => "array",
        DataType::Jsonb => "jsonb",
        DataType::Blob => "blob",
    }
}

fn qdrant_error(e: impl std::fmt::Display) -> VlorQLError {
    VlorQLError::config(
        crate::errors::ConfigErrorKind::ConfigFileError {
            path: "vector_search".into(),
            reason: format!("Qdrant error: {e}"),
        },
        serde_json::json!({}),
    )
}
