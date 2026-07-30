//! Structured, machine-readable errors for the VlorQl core.
//!
//! Every error keeps both a typed error kind and a JSON `details` payload. The
//! typed kind is useful to Rust callers, while the payload allows callers to
//! preserve additional context without parsing an error string.

mod kinds;
mod validation;
mod vlorql_error;

pub use kinds::{
    AuditErrorKind, CompilationErrorKind, ConfigErrorKind, LlmErrorKind, PolicyErrorKind,
    SchemaErrorKind, ValidationErrorKind,
};

pub use vlorql_error::{
    CompilationError, ConfigError, LlmError, PolicyError, SchemaError, ValidationError, VlorQLError,
};

pub use validation::{ValidationErrors, validate};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The response shape exposed by API and LLM-facing layers.
///
/// # Examples
///
/// ```
/// use vlorql_core::errors::ErrorResponse;
/// use serde_json::json;
///
/// let response = ErrorResponse {
///     code: "V001".to_owned(),
///     message: "validation error".to_owned(),
///     details: json!({"field": "from"}),
///     suggestion: Some("add a from clause".to_owned()),
/// };
/// assert_eq!(response.code, "V001");
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable description of the error.
    pub message: String,
    /// Structured context associated with the error.
    pub details: Value,
    /// Optional guidance that can be used to repair the request.
    pub suggestion: Option<String>,
}
