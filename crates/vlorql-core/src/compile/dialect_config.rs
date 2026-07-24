//! Config-driven SQL dialect definitions.

use crate::errors::{ConfigErrorKind, VlorQLError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

/// A configurable SQL dialect definition loaded from TOML/YAML.
#[allow(missing_docs)]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DialectConfig {
    pub name: String,
    pub identifier_quote: String,
    pub placeholder: String,
    pub limit_offset: String,
    pub top_syntax: Option<String>,
    pub supports_cte: bool,
    pub supports_window_functions: bool,
    pub supports_json_operations: bool,
    pub supports_offset: bool,
    pub supports_fetch: bool,
    pub allow_distinct: bool,
    pub allow_select_distinct: bool,
    pub max_joins: Option<usize>,
    pub allowed_join_types: Vec<String>,
    pub allowed_functions: Vec<String>,
    pub denied_functions: Vec<String>,
    pub max_group_by_columns: Option<usize>,
    pub type_mappings: HashMap<String, String>,
    pub function_name_mappings: HashMap<String, String>,
}

impl Default for DialectConfig {
    fn default() -> Self {
        Self::default_postgres()
    }
}

impl DialectConfig {
    pub fn placeholder_str(&self, index: usize) -> String {
        if self.placeholder.contains("{index}") {
            self.placeholder.replace("{index}", &index.to_string())
        } else {
            self.placeholder.clone()
        }
    }

    pub fn quote_identifier(&self, ident: &str) -> String {
        match self.identifier_quote.as_str() {
            "double_quote" => format!("\"{}\"", ident.replace('"', "\"\"")),
            "backtick" => format!("`{}`", ident.replace('`', "``")),
            "bracket" => format!("[{}]", ident),
            "never" => ident.to_string(),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    pub fn render_limit_offset(&self, limit: Option<u64>, offset: Option<u64>) -> Option<String> {
        if limit.is_none() && offset.is_none() {
            return None;
        }
        let template = &self.limit_offset;
        let result = template
            .replace("{limit}", &limit.map(|v| v.to_string()).unwrap_or_default())
            .replace("{offset}", &offset.map(|v| v.to_string()).unwrap_or_default());
        if result.trim().is_empty() {
            return None;
        }
        Some(result)
    }

    pub fn render_top(&self, limit: u64) -> Option<String> {
        self.top_syntax
            .as_ref()
            .map(|s| s.replace("{limit}", &limit.to_string()))
    }

    pub fn default_postgres() -> Self {
        Self {
            name: "postgres".into(),
            identifier_quote: "double_quote".into(),
            placeholder: "${index}".into(),
            limit_offset: "LIMIT {limit} OFFSET {offset}".into(),
            top_syntax: None,
            supports_cte: true,
            supports_window_functions: true,
            supports_json_operations: true,
            supports_offset: true,
            supports_fetch: true,
            allow_distinct: true,
            allow_select_distinct: true,
            max_joins: None,
            allowed_join_types: vec![],
            allowed_functions: vec![],
            denied_functions: vec![],
            max_group_by_columns: None,
            type_mappings: HashMap::new(),
            function_name_mappings: HashMap::new(),
        }
    }

    pub fn default_sqlite() -> Self {
        let mut cfg = Self::default_postgres();
        cfg.name = "sqlite".into();
        cfg.placeholder = "?".into();
        cfg.limit_offset = "LIMIT {limit} OFFSET {offset}".into();
        cfg
    }

    pub fn default_mysql() -> Self {
        let mut type_mappings = HashMap::new();
        type_mappings.insert("ilike".into(), "LIKE".into());
        Self {
            name: "mysql".into(),
            identifier_quote: "backtick".into(),
            placeholder: "?".into(),
            limit_offset: "LIMIT {offset}, {limit}".into(),
            top_syntax: None,
            supports_cte: true,
            supports_window_functions: true,
            supports_json_operations: false,
            supports_offset: true,
            supports_fetch: false,
            allow_distinct: true,
            allow_select_distinct: true,
            max_joins: None,
            allowed_join_types: vec![],
            allowed_functions: vec![],
            denied_functions: vec![],
            max_group_by_columns: None,
            type_mappings,
            function_name_mappings: HashMap::new(),
        }
    }

    pub fn from_toml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
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

    pub fn from_yaml(path: impl AsRef<Path>) -> Result<Self, VlorQLError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_postgres_style() {
        let cfg = DialectConfig::default_postgres();
        assert_eq!(cfg.placeholder_str(1), "$1");
        assert_eq!(cfg.placeholder_str(3), "$3");
    }

    #[test]
    fn placeholder_mysql_style() {
        let cfg = DialectConfig::default_mysql();
        assert_eq!(cfg.placeholder_str(1), "?");
        assert_eq!(cfg.placeholder_str(5), "?");
    }

    #[test]
    fn placeholder_custom_style() {
        let cfg = DialectConfig {
            placeholder: "@p{index}".into(),
            ..DialectConfig::default_postgres()
        };
        assert_eq!(cfg.placeholder_str(1), "@p1");
        assert_eq!(cfg.placeholder_str(42), "@p42");
    }

    #[test]
    fn quote_identifier_double_quote() {
        let cfg = DialectConfig::default_postgres();
        assert_eq!(cfg.quote_identifier("users"), r#""users""#);
        assert_eq!(cfg.quote_identifier(r#"he"llo"#), r#""he""llo""#);
    }

    #[test]
    fn quote_identifier_backtick() {
        let cfg = DialectConfig::default_mysql();
        assert_eq!(cfg.quote_identifier("users"), "`users`");
        assert_eq!(cfg.quote_identifier("order"), "`order`");
    }

    #[test]
    fn quote_identifier_bracket() {
        let cfg = DialectConfig {
            identifier_quote: "bracket".into(),
            ..DialectConfig::default_postgres()
        };
        assert_eq!(cfg.quote_identifier("users"), "[users]");
    }

    #[test]
    fn quote_identifier_never() {
        let cfg = DialectConfig {
            identifier_quote: "never".into(),
            ..DialectConfig::default_postgres()
        };
        assert_eq!(cfg.quote_identifier("users"), "users");
    }

    #[test]
    fn render_limit_offset_both() {
        let cfg = DialectConfig::default_postgres();
        let result = cfg.render_limit_offset(Some(10), Some(20));
        assert_eq!(result.as_deref(), Some("LIMIT 10 OFFSET 20"));
    }

    #[test]
    fn render_limit_offset_limit_only() {
        let cfg = DialectConfig::default_postgres();
        let result = cfg.render_limit_offset(Some(10), None);
        assert_eq!(result.as_deref(), Some("LIMIT 10 OFFSET "));
    }

    #[test]
    fn render_limit_offset_mysql() {
        let cfg = DialectConfig::default_mysql();
        let result = cfg.render_limit_offset(Some(10), Some(20));
        assert_eq!(result.as_deref(), Some("LIMIT 20, 10"));
    }

    #[test]
    fn render_limit_offset_fetch() {
        let cfg = DialectConfig {
            limit_offset: "OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY".into(),
            ..DialectConfig::default_postgres()
        };
        let result = cfg.render_limit_offset(Some(10), Some(0));
        assert_eq!(
            result.as_deref(),
            Some("OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY")
        );
    }

    #[test]
    fn render_top_syntax() {
        let cfg = DialectConfig {
            top_syntax: Some("SELECT TOP {limit}".into()),
            ..DialectConfig::default_postgres()
        };
        assert_eq!(cfg.render_top(5).as_deref(), Some("SELECT TOP 5"));
    }

    #[test]
    fn default_postgres_values() {
        let cfg = DialectConfig::default_postgres();
        assert_eq!(cfg.name, "postgres");
        assert!(cfg.supports_cte);
        assert!(cfg.supports_window_functions);
        assert!(cfg.allow_distinct);
    }

    #[test]
    fn default_mysql_values() {
        let cfg = DialectConfig::default_mysql();
        assert_eq!(cfg.name, "mysql");
        assert_eq!(cfg.identifier_quote, "backtick");
        assert_eq!(cfg.placeholder, "?");
        assert!(!cfg.supports_json_operations);
    }

    #[test]
    fn default_sqlite_values() {
        let cfg = DialectConfig::default_sqlite();
        assert_eq!(cfg.name, "sqlite");
        assert_eq!(cfg.placeholder, "?");
    }

    #[test]
    fn from_toml_loads_config() {
        let toml_str = r#"
name = "testdb"
identifier_quote = "double_quote"
placeholder = "${index}"
limit_offset = "LIMIT {limit} OFFSET {offset}"
supports_cte = true
supports_window_functions = false
supports_json_operations = false
supports_offset = true
supports_fetch = false
allow_distinct = true
allow_select_distinct = true
allowed_join_types = ["inner", "left"]

[type_mappings]
ilike = "LIKE"

[function_name_mappings]
"now" = "GETDATE"
"#;
        let cfg: DialectConfig = toml::from_str(toml_str).expect("should parse");
        assert_eq!(cfg.name, "testdb");
        assert!(!cfg.supports_window_functions);
        assert_eq!(cfg.allowed_join_types, vec!["inner", "left"]);
        assert_eq!(cfg.type_mappings.get("ilike").unwrap(), "LIKE");
        assert_eq!(cfg.function_name_mappings.get("now").unwrap(), "GETDATE");
    }
}
