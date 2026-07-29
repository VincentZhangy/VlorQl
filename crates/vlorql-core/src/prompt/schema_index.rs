//! Vector-based schema retrieval using Qdrant.
//!
//! The SchemaSnapshot itself comes from the database (DDL). This module
//! takes the existing SchemaSnapshot, generates text descriptions for
//! each table/column, embeds them into Qdrant, and provides semantic
//! search to find relevant tables for a user question.

use crate::errors::VlorQLError;
use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex};

/// Indexes schema table/column text descriptions into Qdrant for
/// semantic retrieval. Lazy-initialized on first use.
pub struct SchemaIndexer {
    client: qdrant_client::Qdrant,
    collection_name: String,
}

impl fmt::Debug for SchemaIndexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaIndexer")
            .field("collection_name", &self.collection_name)
            .finish()
    }
}

#[cfg(feature = "vector-search")]
impl SchemaIndexer {
    /// Connect to a running Qdrant instance.
    pub async fn connect(url: &str) -> Result<Self, VlorQLError> {
        let client = qdrant_client::Qdrant::from_url(url)
            .build()
            .map_err(qdrant_error)?;
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
            let params = VectorParamsBuilder::new(EMBEDDING_DIM, Distance::Cosine);
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
                    let name = name.to_owned();
                    if !tables.contains(&name) {
                        tables.push(name);
                    }
                }
            }
        }
        Ok(tables)
    }

    /// Check Qdrant connection health.
    pub async fn health_check(&self) -> Result<(), VlorQLError> {
        self.client.health_check().await
            .map_err(qdrant_error)?;
        Ok(())
    }
}

/// Generate a text description for a table (used for embedding).
pub fn table_to_text(schema: &crate::schema::TableSchema) -> String {
    let cols: Vec<String> = schema.columns.iter()
        .map(|c| format!("{} {}", c.name, c.data_type.type_name()))
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
    format!("Column: {}.{} {}{}", table, column.name, column.data_type.type_name(), desc)
}


/// OpenAI embedding dimension for text-embedding-3-small.
#[cfg(feature = "vector-search")]
const EMBEDDING_DIM: u64 = 512;

/// Embedding cache: input text → embedding vector.
#[cfg(feature = "vector-search")]
static EMBEDDING_CACHE: LazyLock<Mutex<HashMap<String, Vec<f32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Shared HTTP client for OpenAI API calls.
#[cfg(feature = "vector-search")]
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Embed text using OpenAI text-embedding-3-small API.
///
/// Results are cached per unique input text to avoid redundant API calls
/// during schema indexing.
#[cfg(feature = "vector-search")]
async fn embed_text(text: &str) -> Result<Vec<f32>, VlorQLError> {
    const OPENAI_EMBEDDING_URL: &str = "https://api.openai.com/v1/embeddings";
    embed_text_at_url(text, OPENAI_EMBEDDING_URL).await
}

/// Inner function with configurable URL for testing.
#[cfg(feature = "vector-search")]
async fn embed_text_at_url(text: &str, url: &str) -> Result<Vec<f32>, VlorQLError> {
    {
        let cache = EMBEDDING_CACHE.lock().unwrap();
        if let Some(cached) = cache.get(text) {
            return Ok(cached.clone());
        }
    }

    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "OPENAI_API_KEY".into(),
                reason: "OPENAI_API_KEY environment variable not set".into(),
            },
            serde_json::json!({}),
        )
    })?;

    let client = &*HTTP_CLIENT;
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": text,
        }))
        .send()
        .await
        .map_err(|e| {
            VlorQLError::config(
                crate::errors::ConfigErrorKind::ConfigFileError {
                    path: "vector_search".into(),
                    reason: format!("OpenAI embedding request failed: {e}"),
                },
                serde_json::json!({}),
            )
        })?;

    let body: serde_json::Value = resp.json().await.map_err(|e| {
        VlorQLError::config(
            crate::errors::ConfigErrorKind::ConfigFileError {
                path: "vector_search".into(),
                reason: format!("OpenAI embedding parse failed: {e}"),
            },
            serde_json::json!({}),
        )
    })?;

    let vector = parse_embedding_response(body)?;

    {
        let mut cache = EMBEDDING_CACHE.lock().unwrap();
        cache.insert(text.to_owned(), vector.clone());
    }

    Ok(vector)
}

/// Extract the embedding vector from an OpenAI-compatible JSON response.
#[cfg(feature = "vector-search")]
fn parse_embedding_response(body: serde_json::Value) -> Result<Vec<f32>, VlorQLError> {
    let vector: Vec<f32> = body["data"][0]["embedding"]
        .as_array()
        .ok_or_else(|| {
            VlorQLError::config(
                crate::errors::ConfigErrorKind::ConfigFileError {
                    path: "vector_search".into(),
                    reason: "OpenAI embedding response missing embedding field".into(),
                },
                serde_json::json!({}),
            )
        })?
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();
    Ok(vector)
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

#[cfg(test)]
#[cfg(feature = "vector-search")]
mod tests {
    use super::*;

    #[test]
    fn test_parse_embedding_success() {
        let body = serde_json::json!({
            "data": [{"embedding": [0.1, 0.2, 0.3]}]
        });
        let result = parse_embedding_response(body);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn test_parse_embedding_missing_field() {
        let body = serde_json::json!({
            "data": [{"foo": "bar"}]
        });
        let result = parse_embedding_response(body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_embedding_missing_data() {
        let body = serde_json::json!({});
        let result = parse_embedding_response(body);
        assert!(result.is_err());
    }
}
