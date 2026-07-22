//! Centralized function registry.
//!
//! Defines function metadata (name, kind, arity, types, dialects) in a
//! single place so that every downstream layer (normalize, validate,
//! optimize, fixer, compiler) can discover function properties without
//! hardcoded lists.
//!
//! # Quick start
//!
//! ```ignore
//! // At startup (e.g. in vlorql_core::init):
//! vlorql_core::function::init_registry(vlorql_core::function::builtin_functions());
//!
//! // Then anywhere:
//! let def = vlorql_core::function::lookup_function("sum").unwrap();
//! assert!(vlorql_core::function::is_aggregate("count"));
//! ```

pub mod builder;
pub mod builtin;
pub mod def;
pub mod registry;

pub use def::{Dialect, FunctionDef, FunctionKind};
pub use registry::{
    init_registry, is_aggregate, is_known_function, lookup_function,
    lookup_function_for_dialect, register_function,
};