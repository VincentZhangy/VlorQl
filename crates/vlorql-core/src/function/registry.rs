//! Global function registry with runtime registration support.
//!
//! Uses a `OnceLock<RwLock<HashMap>>` so that built-in functions are
//! loaded at startup and user-defined functions (UDFs) can be added
//! at runtime without sacrificing thread safety.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use super::def::{Dialect, FunctionDef, FunctionKind};

static REGISTRY: OnceLock<RwLock<HashMap<String, FunctionDef>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, FunctionDef>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Initialise the registry with a set of built-in functions.
///
/// Should be called once during program startup (e.g. from
/// `vlorql_core::init_function_registry()`).
pub fn init_registry(builtin: Vec<FunctionDef>) {
    let mut guard = registry().write().expect("function registry lock poisoned");
    for def in builtin {
        for name in &def.names {
            guard.insert(name.to_string(), def.clone());
        }
    }
}

/// Register a single function at runtime (e.g. a user-defined function).
///
/// Returns an error if any name is already registered.
pub fn register_function(def: FunctionDef) -> Result<(), String> {
    let mut guard = registry().write().expect("function registry lock poisoned");
    for name in &def.names {
        if guard.contains_key(name.as_ref()) {
            return Err(format!("Function '{}' is already registered", name));
        }
    }
    for name in &def.names {
        guard.insert(name.to_string(), def.clone());
    }
    Ok(())
}

/// Look up a function by name (case-insensitive).
///
/// Returns a cloned `FunctionDef` so the caller does not hold a read lock.
pub fn lookup_function(name: &str) -> Option<FunctionDef> {
    let guard = registry().read().expect("function registry lock poisoned");
    // Exact match first (fast path).
    if let Some(def) = guard.get(name) {
        return Some(def.clone());
    }
    // Case-insensitive fallback.
    for (key, def) in guard.iter() {
        if key.eq_ignore_ascii_case(name) {
            return Some(def.clone());
        }
    }
    None
}

/// Look up a function by name, filtering by dialect.
pub fn lookup_function_for_dialect(name: &str, dialect: Dialect) -> Option<FunctionDef> {
    lookup_function(name).and_then(|def| {
        if def.supports_dialect(dialect) {
            Some(def)
        } else {
            None
        }
    })
}

/// Returns `true` when `name` is a known aggregate function.
pub fn is_aggregate(name: &str) -> bool {
    lookup_function(name)
        .is_some_and(|def| def.kind == FunctionKind::Aggregate)
}

/// Returns `true` when `name` is a known function of any kind.
pub fn is_known_function(name: &str) -> bool {
    lookup_function(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::builder::FunctionDefBuilder;

    fn setup() {
        let _ = init_registry(vec![
            FunctionDefBuilder::new("sum")
                .kind(FunctionKind::Aggregate)
                .min_args(1)
                .build(),
            FunctionDefBuilder::new("upper")
                .alias("ucase")
                .kind(FunctionKind::Scalar)
                .min_args(1)
                .build(),
        ]);
    }

    #[test]
    fn lookup_known_function() {
        setup();
        let def = lookup_function("sum").expect("sum should be found");
        assert_eq!(def.kind, FunctionKind::Aggregate);
    }

    #[test]
    fn lookup_case_insensitive() {
        setup();
        assert!(lookup_function("SUM").is_some());
        assert!(lookup_function("Upper").is_some());
    }

    #[test]
    fn lookup_unknown_returns_none() {
        setup();
        assert!(lookup_function("nonexistent").is_none());
    }

    #[test]
    fn alias_resolves() {
        setup();
        let def = lookup_function("ucase").expect("ucase alias should resolve");
        assert_eq!(def.canonical_name(), "upper");
    }

    #[test]
    fn is_aggregate_works() {
        setup();
        assert!(is_aggregate("sum"));
        assert!(!is_aggregate("upper"));
    }

    #[test]
    fn register_then_lookup() {
        setup();
        let def = FunctionDefBuilder::new("my_udf")
            .kind(FunctionKind::Scalar)
            .min_args(2)
            .build();
        register_function(def).unwrap();
        assert!(lookup_function("my_udf").is_some());
    }

    #[test]
    fn duplicate_registration_fails() {
        setup();
        let def = FunctionDefBuilder::new("sum").build();
        assert!(register_function(def).is_err());
    }
}