//! SQL rewrite engine for post-compilation transformations.

use crate::errors::{ConfigErrorKind, VlorQLError};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

/// One rewrite rule: match with regex, replace with template.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RewriteRule {
    pub name: String,
    pub description: Option<String>,
    pub match_pattern: String,
    pub replace_template: String,
    pub dialect_filter: Option<Vec<String>>,
}

/// Applies a set of rewrite rules to generated SQL.
#[allow(missing_docs)]
#[derive(Debug, Default)]
pub struct RewriteEngine {
    rules: Vec<RewriteRule>,
}

impl RewriteEngine {
    pub fn new(rules: Vec<RewriteRule>) -> Self {
        Self { rules }
    }

    pub fn apply(&self, sql: &str, dialect: &str) -> Result<String, VlorQLError> {
        let mut result = sql.to_string();
        for rule in &self.rules {
            if let Some(filter) = &rule.dialect_filter {
                if !filter.iter().any(|d| d.eq_ignore_ascii_case(dialect)) {
                    continue;
                }
            }
            let re = Regex::new(&rule.match_pattern).map_err(|e| {
                VlorQLError::config(
                    ConfigErrorKind::ConfigFileError {
                        path: format!("rule:{}", rule.name),
                        reason: format!("invalid regex: {e}"),
                    },
                    json!({"rule": rule.name, "pattern": &rule.match_pattern}),
                )
            })?;
            result = re
                .replace_all(&result, rule.replace_template.as_str())
                .to_string();
        }
        Ok(result)
    }

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
        #[derive(Deserialize)]
        struct RulesFile {
            rules: Vec<RewriteRule>,
        }
        let parsed: RulesFile = toml::from_str(&content).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string(), "parse_error": e.to_string()}),
            )
        })?;
        Ok(Self::new(parsed.rules))
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
        #[derive(Deserialize)]
        struct RulesFile {
            rules: Vec<RewriteRule>,
        }
        let parsed: RulesFile = serde_yaml::from_str(&content).map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::ConfigFileError {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                },
                json!({"path": path.display().to_string(), "parse_error": e.to_string()}),
            )
        })?;
        Ok(Self::new(parsed.rules))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_ilike_to_lower() {
        let engine = RewriteEngine::new(vec![RewriteRule {
            name: "ilike_to_lower".into(),
            description: None,
            match_pattern: r"(?P<left>\S+)\s+ILIKE\s+(?P<right>\S+)".into(),
            replace_template: "LOWER(${left}) LIKE LOWER(${right})".into(),
            dialect_filter: Some(vec!["mysql".into()]),
        }]);
        let sql = engine
            .apply("WHERE name ILIKE '%foo%'", "mysql")
            .unwrap();
        assert_eq!(sql, "WHERE LOWER(name) LIKE LOWER('%foo%')");
    }

    #[test]
    fn rewrite_skipped_for_wrong_dialect() {
        let engine = RewriteEngine::new(vec![RewriteRule {
            name: "ilike_to_lower".into(),
            description: None,
            match_pattern: "ILIKE".into(),
            replace_template: "LIKE".into(),
            dialect_filter: Some(vec!["mysql".into()]),
        }]);
        let sql = engine
            .apply("WHERE name ILIKE '%foo%'", "postgres")
            .unwrap();
        assert_eq!(sql, "WHERE name ILIKE '%foo%'");
    }

    #[test]
    fn rewrite_multiple_rules() {
        let engine = RewriteEngine::new(vec![
            RewriteRule {
                name: "ilike".into(),
                description: None,
                match_pattern: "ILIKE".into(),
                replace_template: "LIKE".into(),
                dialect_filter: None,
            },
            RewriteRule {
                name: "now".into(),
                description: None,
                match_pattern: r"\bNOW\(\)".into(),
                replace_template: "GETDATE()".into(),
                dialect_filter: None,
            },
        ]);
        let sql = engine
            .apply("WHERE name ILIKE '%foo%' AND date = NOW()", "mssql")
            .unwrap();
        assert_eq!(sql, "WHERE name LIKE '%foo%' AND date = GETDATE()");
    }

    #[test]
    fn rewrite_no_rules_passthrough() {
        let engine = RewriteEngine::new(vec![]);
        let sql = engine.apply("SELECT 1", "postgres").unwrap();
        assert_eq!(sql, "SELECT 1");
    }

    #[test]
    fn rewrite_invalid_regex_returns_error() {
        let engine = RewriteEngine::new(vec![RewriteRule {
            name: "bad".into(),
            description: None,
            match_pattern: r"[invalid".into(),
            replace_template: "x".into(),
            dialect_filter: None,
        }]);
        assert!(engine.apply("test", "postgres").is_err());
    }
}
