//! Expression builder: canonical JSON → [`Expression`](vlorql_core::schema::Expression) / [`Predicate`](vlorql_core::schema::Predicate).
//!
//! Builds typed AST nodes from canonical JSON.  This layer does **no**
//! repair — it assumes the input has already been normalized.

use std::fmt;
use thiserror::Error;

/// Error returned when building an AST node from JSON fails.
#[derive(Debug, Clone, Error)]
pub struct BuildError {
    /// JSON path to the field that caused the error (empty for root-level errors).
    path: String,
    /// Human-readable error description.
    message: String,
}

impl BuildError {
    /// Create a new error at the current path.
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Prepend a field name to the error path.
    pub fn at(self, field: &str) -> Self {
        let new_path = if self.path.is_empty() {
            field.to_owned()
        } else {
            format!("{}.{}", field, self.path)
        };
        Self {
            path: new_path,
            message: self.message,
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "at `{}`: {}", self.path, self.message)
        }
    }
}

impl From<BuildError> for serde_json::Error {
    fn from(e: BuildError) -> Self {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    }
}

/// Field extraction helpers and operator/type parsers.
pub mod helpers;
/// Expression and Predicate builders.
pub mod builders;

pub use helpers::{
    req_str, opt_str, req_obj, req_arr, type_name,
    parse_comparison_op, parse_binary_op, parse_join_type, parse_data_type,
};
pub use builders::{build_expression, build_predicate};

#[cfg(test)]
mod tests;
