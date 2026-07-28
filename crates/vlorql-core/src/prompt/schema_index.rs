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
    pub async fn index_schema(&self, schema: &crate::schema::SchemaSnapshot) -> Result<(), VlorQLError> {
        use qdrant_client::qdrant::{
            CreateCollectionBuilder, Distance, VectorParamsBuilder,
            PointStruct, UpsertPointsBuilder,
        };
        use qdrant_client::Payload;

        // 1. Ensure collection exists
        let collections = self.client.list_collections().await
            .map_err(qdrant_error)?;
        let exists = collections.collections.iter()
            .any(|c| c.name == self.collection_name);
        if !exists {
            let params = VectorParamsBuilder::new(384, Distance::Cosine);
            self.client.create_collection(
                CreateCollectionBuilder::new(self.collection_name.clone())
                    .vectors_config(params)
            ).await.map_err(qdrant_error)?;
        }

        // 2. Generate points for each table and column
        let mut points = Vec::new();
        let mut id: u64 = 0;
        for table in &schema.tables {
            let text = table_to_text(table);
            let embedding = embed_text(&text).await?;
            let mut payload = Payload::new();
            payload.insert("type", "table");
            payload.insert("name", table.name.clone());
            payload.insert("text", text);
            points.push(
                PointStruct::new(id, embedding, payload)
            );
            id += 1;

            for column in &table.columns {
                let text = column_to_text(&table.name, column);
                let embedding = embed_text(&text).await?;
                let mut payload = Payload::new();
                payload.insert("type", "column");
                payload.insert("table", table.name.clone());
                payload.insert("column", column.name.clone());
                payload.insert("text", text);
                points.push(
                    PointStruct::new(id, embedding, payload)
                );
                id += 1;
            }
        }

        // 3. Upload points in batches
        if !points.is_empty() {
            self.client.upsert_points(
                UpsertPointsBuilder::new(self.collection_name.clone(), points)
            ).await.map_err(qdrant_error)?;
        }

        Ok(())
    }

    /// Search for tables semantically relevant to the user question.
    pub async fn search(&self, question: &str, top_k: u64) -> Result<Vec<String>, VlorQLError> {
        use qdrant_client::qdrant::QueryPointsBuilder;

        let query_vector = embed_text(question).await?;
        let result = self.client.query(
            QueryPointsBuilder::new(self.collection_name.clone())
                .query(query_vector)
                .limit(top_k)
                .with_payload(true)
        ).await.map_err(qdrant_error)?;

        let mut tables: Vec<String> = Vec::new();
        for point in result.result {
            let payload = &point.payload;
            // Table points have "name", column points have "table"
            for key in &["name", "table"] {
                if let Some(name) = payload.get(*key).and_then(|v| v.as_str()) {
                    if !tables.contains(&name.to_owned()) {
                        tables.push(name.to_owned());
                    }
                }
            }
        }
        Ok(tables)
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
    format!("Column: {}.{} {}{}", table, column.name, data_type_name(column.data_type), desc)
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

/// Embed text to a vector. Currently uses a zero-vector placeholder.
/// In production, replace this with all-MiniLM-L6-v2 or an embedding API.
async fn embed_text(_text: &str) -> Result<Vec<f32>, VlorQLError> {
    // TODO: Replace with real embedding (ONNX runtime or API call)
    Ok(vec![0.0f32; 384])
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
