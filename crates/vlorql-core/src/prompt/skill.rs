//! Prompt skills for custom LLM instruction injection.
#![allow(missing_docs)]

use crate::errors::{ConfigErrorKind, VlorQLError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

/// A pair of (question, expected plan) for few-shot examples.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExamplePair {
    pub question: String,
    pub plan: serde_json::Value,
}

/// A skill that injects custom instructions, simplifies schema, or adds examples.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptSkill {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Vec<String>,
    pub simplify_schema: bool,
    pub forbid_features: Vec<String>,
    pub disable_output_fields: Vec<String>,
    pub examples: Vec<ExamplePair>,
}

impl PromptSkill {
    pub fn load_toml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string()}),
            )
        })?;
        toml::from_str(&content).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string(), "parse_error": e.to_string()}),
            )
        })
    }

    pub fn load_yaml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string()}),
            )
        })?;
        serde_yaml::from_str(&content).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string(), "parse_error": e.to_string()}),
            )
        })
    }

    pub fn builtin_small_model() -> Self {
        Self {
            name: "small-model".into(),
            description: Some("Simplifies output schema for small / local LLMs".into()),
            instructions: vec![
                "Keep queries simple: max 2 joins, no subqueries in WHERE.".into(),
                "Always use table aliases (first letter of table name).".into(),
                "Prefer LEFT JOIN over NOT IN / NOT EXISTS.".into(),
                "GROUP BY must include every non-aggregated column in SELECT.".into(),
            ],
            simplify_schema: true,
            forbid_features: vec![
                "set_operation".into(),
                "window_functions".into(),
                "ctes".into(),
                "distinct_on".into(),
            ],
            disable_output_fields: vec![
                "set_operation".into(),
                "ctes".into(),
                "distinct_on".into(),
            ],
            examples: vec![ExamplePair {
                question: "Show user names".into(),
                plan: serde_json::json!({
                    "select": [{"type": "column_ref", "table": "users", "column": "name", "alias": null}],
                    "from": {"table": "users", "alias": null}
                }),
            }],
        }
    }
}
