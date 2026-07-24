//! Dialect registry for builtin and custom SQL compilers.

use std::collections::HashMap;
use std::sync::Arc;

use super::dialect_config::DialectConfig;
use super::{CompiledQuery, MySQLCompiler, PostgresCompiler, SQLiteCompiler, SqlCompiler};
use crate::errors::{ConfigErrorKind, VlorQLError};
use crate::schema::SqlDialect;
use serde_json::json;

/// Stateless factory that returns builtin compilers by enum.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompilerRegistry;

impl CompilerRegistry {
    pub fn get(dialect: SqlDialect) -> Box<dyn SqlCompiler> {
        get_compiler(dialect)
    }
}

pub fn get_compiler(dialect: SqlDialect) -> Box<dyn SqlCompiler> {
    match dialect {
        SqlDialect::Postgres => Box::new(PostgresCompiler),
        SqlDialect::Sqlite => Box::new(SQLiteCompiler),
        SqlDialect::MySql => Box::new(MySQLCompiler),
    }
}

#[allow(missing_docs)]
#[derive(Debug, Default)]
pub struct DialectRegistry {
    custom: HashMap<String, Arc<DialectConfig>>,
}

impl DialectRegistry {
    pub fn new() -> Self {
        Self {
            custom: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, config: DialectConfig) -> Result<(), VlorQLError> {
        let name_lower = name.to_lowercase();
        if matches!(
            name_lower.as_str(),
            "postgres" | "postgresql" | "sqlite" | "mysql" | "my_sql"
        ) {
            return Err(VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: name.to_owned(),
                },
                json!({"reason": "cannot override builtin dialect", "dialect": name}),
            ));
        }
        self.custom.insert(name_lower, Arc::new(config));
        Ok(())
    }

    pub fn get_config(&self, name: &str) -> Option<Arc<DialectConfig>> {
        let name = name.to_lowercase();
        match name.as_str() {
            "postgres" | "postgresql" => Some(Arc::new(DialectConfig::default_postgres())),
            "sqlite" => Some(Arc::new(DialectConfig::default_sqlite())),
            "mysql" | "my_sql" => Some(Arc::new(DialectConfig::default_mysql())),
            _ => self.custom.get(&name).cloned(),
        }
    }

    pub fn get_compiler(&self, name: &str) -> Result<Box<dyn SqlCompiler>, VlorQLError> {
        let name_lower = name.to_lowercase();
        match name_lower.as_str() {
            "postgres" | "postgresql" => Ok(Box::new(PostgresCompiler)),
            "sqlite" => Ok(Box::new(SQLiteCompiler)),
            "mysql" | "my_sql" => Ok(Box::new(MySQLCompiler)),
            _ => {
                let config = self.custom.get(&name_lower).ok_or_else(|| {
                    VlorQLError::config(
                        ConfigErrorKind::InvalidDialect {
                            dialect: name.to_owned(),
                        },
                        json!({"accepted": ["postgres", "sqlite", "mysql"], "custom_registered": self.custom.keys().collect::<Vec<_>>()}),
                    )
                })?;
                Ok(Box::new(ConfigCompiler(config.clone())))
            }
        }
    }
}

#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct ConfigCompiler(pub Arc<DialectConfig>);

impl SqlCompiler for ConfigCompiler {
    fn compile(&self, plan: &crate::validate::ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let (sql, parameters) =
            super::QueryBuilder::new(plan, &self.0).build()?;
        Ok(CompiledQuery {
            sql,
            parameters,
            dialect: SqlDialect::Postgres,
        })
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_registry_returns_builtin() {
        let registry = DialectRegistry::new();
        let pg = registry.get_config("postgres").expect("postgres");
        assert_eq!(pg.name, "postgres");
        let sqlite = registry.get_config("sqlite").expect("sqlite");
        assert_eq!(sqlite.placeholder, "?");
        let mysql = registry.get_config("mysql").expect("mysql");
        assert_eq!(mysql.identifier_quote, "backtick");
    }

    #[test]
    fn dialect_registry_accepts_custom() {
        let mut registry = DialectRegistry::new();
        let cfg = DialectConfig {
            name: "mssql".into(),
            placeholder: "@p{index}".into(),
            ..DialectConfig::default_postgres()
        };
        registry.register("mssql", cfg.clone()).expect("register");
        let got = registry.get_config("mssql").expect("get");
        assert_eq!(got.placeholder_str(1), "@p1");
    }

    #[test]
    fn dialect_registry_rejects_overriding_builtin() {
        let mut registry = DialectRegistry::new();
        let err = registry
            .register("postgres", DialectConfig::default_postgres())
            .unwrap_err();
        assert!(err.to_string().contains("dialect"));
    }

    #[test]
    fn dialect_registry_unknown_returns_none() {
        let registry = DialectRegistry::new();
        assert!(registry.get_config("unknown").is_none());
    }

    #[test]
    fn registry_get_compiler_returns_compilers() {
        let registry = DialectRegistry::new();
        assert!(registry.get_compiler("postgres").is_ok());
        assert!(registry.get_compiler("sqlite").is_ok());
        assert!(registry.get_compiler("mysql").is_ok());
    }

    #[test]
    fn registry_get_compiler_returns_config_compiler() {
        let mut registry = DialectRegistry::new();
        registry
            .register(
                "mssql",
                DialectConfig {
                    placeholder: "?".into(),
                    ..DialectConfig::default_postgres()
                },
            )
            .expect("register");
        let compiler = registry.get_compiler("mssql").expect("compiler");
        assert_eq!(compiler.dialect(), SqlDialect::Postgres);
    }
}
