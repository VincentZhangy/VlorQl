//! Structured, machine-readable errors for the VlorQl core.
//!
//! Every error keeps both a typed error kind and a JSON `details` payload. The
//! typed kind is useful to Rust callers, while the payload allows callers to
//! preserve additional context without parsing an error string.

mod kinds;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub use kinds::{
    CompilationErrorKind, ConfigErrorKind, LlmErrorKind, PolicyErrorKind, SchemaErrorKind,
    ValidationErrorKind,
};

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

/// A structured error from the VlorQl core.
///
/// Every variant carries a structured error kind and a JSON `details`
/// payload so callers can branch on the specific failure without
/// parsing the error message.
///
/// # Examples
///
/// ```
/// use vlorql_core::errors::{VlorQLError, ValidationErrorKind};
/// use serde_json::json;
///
/// let err = VlorQLError::validation(
///     ValidationErrorKind::InvalidJson,
///     json!({"response": "not json"}),
/// );
/// assert_eq!(err.error_code(), "V001");
/// assert!(err.is_retryable());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Error)]
pub enum VlorQLError {
    /// The query plan failed structural or semantic validation.
    #[error("validation error: {kind}")]
    Validation {
        /// The typed validation failure.
        kind: ValidationErrorKind,
        /// Structured context (table/column names, expected types, …).
        details: Value,
    },
    /// The query plan violates an access-control policy.
    #[error("policy violation: {kind}")]
    Policy {
        /// The typed policy violation.
        kind: PolicyErrorKind,
        /// Structured context (table/column names, reason, …).
        details: Value,
    },
    /// The validated plan could not be compiled into SQL.
    #[error("compilation error: {kind}")]
    Compilation {
        /// The typed compilation failure.
        kind: CompilationErrorKind,
        /// Structured context (offending feature, placeholder index, …).
        details: Value,
    },
    /// The schema cannot satisfy the query plan.
    #[error("schema error: {kind}")]
    Schema {
        /// The typed schema failure.
        kind: SchemaErrorKind,
        /// Structured context (missing table/column, available alternatives, …).
        details: Value,
    },
    /// An LLM provider failed or returned an unusable response.
    #[error("LLM error: {kind}")]
    Llm {
        /// The typed LLM failure.
        kind: LlmErrorKind,
        /// Structured context (HTTP status, body fragment, …).
        details: Value,
    },
    /// VlorQl is not configured sufficiently to perform the operation.
    #[error("configuration error: {kind}")]
    Config {
        /// The typed configuration failure.
        kind: ConfigErrorKind,
        /// Structured context (missing field, offending value, …).
        details: Value,
    },
}

impl VlorQLError {
    /// Creates a validation error from any serializable details value.
    pub fn validation<T: Serialize>(kind: ValidationErrorKind, details: T) -> Self {
        Self::Validation {
            kind,
            details: details_to_value(details),
        }
    }

    /// Creates a policy error from any serializable details value.
    pub fn policy<T: Serialize>(kind: PolicyErrorKind, details: T) -> Self {
        Self::Policy {
            kind,
            details: details_to_value(details),
        }
    }

    /// Creates a compilation error from any serializable details value.
    pub fn compilation<T: Serialize>(kind: CompilationErrorKind, details: T) -> Self {
        Self::Compilation {
            kind,
            details: details_to_value(details),
        }
    }

    /// Creates a schema error from any serializable details value.
    pub fn schema<T: Serialize>(kind: SchemaErrorKind, details: T) -> Self {
        Self::Schema {
            kind,
            details: details_to_value(details),
        }
    }

    /// Creates an LLM error from any serializable details value.
    pub fn llm<T: Serialize>(kind: LlmErrorKind, details: T) -> Self {
        Self::Llm {
            kind,
            details: details_to_value(details),
        }
    }

    /// Creates a configuration error from any serializable details value.
    pub fn config<T: Serialize>(kind: ConfigErrorKind, details: T) -> Self {
        Self::Config {
            kind,
            details: details_to_value(details),
        }
    }

    /// Returns a stable code for this error category and kind.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Validation { kind, .. } => match kind {
                ValidationErrorKind::InvalidJson => "V001",
                ValidationErrorKind::MissingField { .. } => "V002",
                ValidationErrorKind::InvalidTable { .. } => "V003",
                ValidationErrorKind::InvalidColumn { .. } => "V004",
                ValidationErrorKind::InvalidFunction { .. } => "V005",
                ValidationErrorKind::TypeMismatch { .. } => "V006",
                ValidationErrorKind::DialectFeatureDisabled { .. } => "V007",
                ValidationErrorKind::TooManyJoins { .. } => "V008",
                ValidationErrorKind::AggregationMismatch { .. } => "V009",
                ValidationErrorKind::MultipleErrors { .. } => "V010",
            },
            Self::Policy { kind, .. } => match kind {
                PolicyErrorKind::TableDenied { .. } => "P001",
                PolicyErrorKind::ColumnDenied { .. } => "P002",
            },
            Self::Compilation { kind, .. } => match kind {
                CompilationErrorKind::UnsupportedDialectFeature { .. } => "C001",
                CompilationErrorKind::InvalidPlaceholder { .. } => "C002",
            },
            Self::Schema { kind, .. } => match kind {
                SchemaErrorKind::TableNotFound { .. } => "S001",
                SchemaErrorKind::ColumnNotFound { .. } => "S002",
                SchemaErrorKind::TableNotInScope { .. } => "S003",
            },
            Self::Llm { kind, .. } => match kind {
                LlmErrorKind::ApiError { .. } => "L001",
                LlmErrorKind::Timeout => "L002",
                LlmErrorKind::ParseError { .. } => "L003",
            },
            Self::Config { kind, .. } => match kind {
                ConfigErrorKind::MissingLlmClient => "G001",
                ConfigErrorKind::MissingSchema => "G002",
                ConfigErrorKind::InvalidDialect { .. } => "G003",
                ConfigErrorKind::MissingApiKey { .. } => "G004",
                ConfigErrorKind::EmptyModel => "G005",
                ConfigErrorKind::ConfigFileError { .. } => "G006",
            },
        }
    }

    /// Converts this error into a machine-readable response with repair guidance.
    pub fn to_error_response(&self) -> ErrorResponse {
        // Record the error in the current span context so it can be
        // correlated with the request's trace_id in the tracing backend.
        tracing::error!(
            error.code = %self.error_code(),
            error.message = %self,
            "Error response generated for {}",
            self.error_code(),
        );
        ErrorResponse {
            code: self.error_code().to_owned(),
            message: self.to_string(),
            details: self.details().clone(),
            suggestion: self.suggestion(),
        }
    }

    /// Returns whether asking the LLM to produce a corrected request can help.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Validation { kind, .. } => matches!(
                kind,
                ValidationErrorKind::InvalidJson
                    | ValidationErrorKind::MissingField { .. }
                    | ValidationErrorKind::InvalidTable { .. }
                    | ValidationErrorKind::InvalidColumn { .. }
                    | ValidationErrorKind::InvalidFunction { .. }
                    | ValidationErrorKind::TypeMismatch { .. }
                    | ValidationErrorKind::AggregationMismatch { .. }
            ),
            Self::Schema { kind, .. } => matches!(
                kind,
                SchemaErrorKind::TableNotFound { .. }
                    | SchemaErrorKind::TableNotInScope { .. }
                    | SchemaErrorKind::ColumnNotFound { .. }
            ),
            Self::Llm { .. } => true,
            _ => false,
        }
    }

    /// Returns the structured details payload.
    pub fn details(&self) -> &Value {
        match self {
            Self::Validation { details, .. }
            | Self::Policy { details, .. }
            | Self::Compilation { details, .. }
            | Self::Schema { details, .. }
            | Self::Llm { details, .. }
            | Self::Config { details, .. } => details,
        }
    }

    /// Returns the typed error category without exposing the JSON details.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Validation { .. } => "validation",
            Self::Policy { .. } => "policy",
            Self::Compilation { .. } => "compilation",
            Self::Schema { .. } => "schema",
            Self::Llm { .. } => "llm",
            Self::Config { .. } => "config",
        }
    }

    fn suggestion(&self) -> Option<String> {
        match self {
            Self::Validation { kind, .. } => match kind {
                ValidationErrorKind::InvalidJson => {
                    Some("Return a JSON object matching the query plan schema.".to_owned())
                }
                ValidationErrorKind::MissingField { field } => Some(format!(
                    "Add the required field `{field}` to the query plan."
                )),
                ValidationErrorKind::InvalidTable {
                    table,
                    available_tables,
                } => available_values_suggestion(
                    format!("Replace table `{table}` with an available table"),
                    available_tables,
                ),
                ValidationErrorKind::InvalidColumn {
                    table,
                    column,
                    available_columns,
                } => available_values_suggestion(
                    format!("Replace column `{table}.{column}` with an available column"),
                    available_columns,
                ),
                ValidationErrorKind::InvalidFunction {
                    function,
                    allowed_functions,
                } => available_values_suggestion(
                    format!("Replace function `{function}` with an allowed function"),
                    allowed_functions,
                ),
                ValidationErrorKind::TypeMismatch {
                    expected,
                    found,
                    expr,
                } => Some(format!(
                    "Change `{expr}` from type `{found}` to the expected type `{expected}`."
                )),
                ValidationErrorKind::DialectFeatureDisabled { feature } => Some(format!(
                    "Remove `{feature}` or select a dialect profile that enables it."
                )),
                ValidationErrorKind::TooManyJoins { actual, max } => Some(format!(
                    "Reduce the query from {actual} joins to at most {max} joins."
                )),
                ValidationErrorKind::AggregationMismatch { message } => Some(format!(
                    "Adjust the selected and grouped expressions to satisfy aggregation rules: {message}"
                )),
                ValidationErrorKind::MultipleErrors { .. } => {
                    Some("Fix each listed validation error and resubmit.".to_owned())
                }
            },
            Self::Policy { .. } => {
                Some("Request the required access or remove the unauthorized resource.".to_owned())
            }
            Self::Compilation { .. } => {
                Some("Use only features supported by the selected SQL dialect compiler.".to_owned())
            }
            Self::Schema { kind, .. } => match kind {
                SchemaErrorKind::TableNotFound { table } => {
                    let tip = if table == "where" || table == "from" {
                        "The 'table' field contains a reserved word or structural field name, not an actual table. Use a valid table name from the schema."
                    } else {
                        "Add the table as a JOIN (with an ON clause) or as the FROM source. If you reference columns with 'table: \"<name>\"', that table must be in FROM or JOINs."
                    };
                    Some(tip.to_owned())
                }
                SchemaErrorKind::TableNotInScope { table } => {
                    let tip = if table == "where" || table == "from" {
                        "The 'table' field contains a reserved word or structural field name, not an actual table. Use a valid table name from the schema."
                    } else {
                        "The table exists in the schema but is not part of the query's FROM or JOIN clauses. Add a JOIN (with an ON clause) for this table."
                    };
                    Some(tip.to_owned())
                }
                SchemaErrorKind::ColumnNotFound { .. } => Some(
                    "Use only column names listed in the schema for the referenced table."
                        .to_owned(),
                ),
            },
            Self::Llm { kind, .. } => match kind {
                LlmErrorKind::ApiError { status, .. } if *status == 401 || *status == 403 => {
                    Some("Check the LLM provider credentials and permissions.".to_owned())
                }
                LlmErrorKind::ApiError { .. } => Some(
                    "Retry the LLM request with backoff, then inspect the provider status."
                        .to_owned(),
                ),
                LlmErrorKind::Timeout => {
                    Some("Retry the LLM request or increase the request timeout.".to_owned())
                }
                LlmErrorKind::ParseError { details } => {
                    let details_lower = details.to_lowercase();
                    let tip = if details_lower.contains("where")
                        && (details_lower.contains("array")
                            || details_lower.contains("sequence")
                            || details_lower.contains("list")
                            || details_lower.contains("expected"))
                    {
                        "The 'where' field must be a single Predicate object (NOT an array). In 'and'/'or', each of 'left' and 'right' is a single Predicate {...} — never wrap them in [...]."
                    } else if details_lower.contains("unknown field") {
                        "Remove any unrecognized fields from the JSON. Only fields defined in the schema are allowed."
                    } else if details_lower.contains("invalid type")
                        && details_lower.contains("expected")
                    {
                        "A field has the wrong JSON type — e.g., an array where an object was expected, or a string where a number was expected. Check the field types in the schema."
                    } else if details_lower.contains("expected struct") {
                        "A field contains a string instead of a JSON object, or a JSON array has an element of the wrong type. Ensure all nested objects use {...} not \"...\"."
                    } else if details_lower.contains("expected variant")
                        || details_lower.contains("unknown variant")
                    {
                        "The 'type' field has an unrecognized value. Use only valid type tags: 'column_ref', 'literal', 'comparison', 'and', 'or', etc."
                    } else if details_lower.contains("missing field") {
                        "A required field is missing. Add the required field to the JSON object."
                    } else if details_lower.contains("trailing characters")
                        || details_lower.contains("control character")
                        || details_lower.contains("escape")
                        || details_lower.contains("expected")
                    {
                        "The response contains invalid JSON syntax. Return ONLY a raw JSON object — no markdown fences (```json), no extra text before or after."
                    } else {
                        "Return only a JSON object matching the QueryPlan schema. No markdown fences, no extra text, no comments."
                    };
                    Some(tip.to_owned())
                }
            },
            Self::Config { .. } => None,
        }
    }
}

/// A convenient constructor namespace for validation errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidationError;

impl ValidationError {
    /// Builds a [`VlorQLError::Validation`] value.
    pub fn error<T: Serialize>(kind: ValidationErrorKind, details: T) -> VlorQLError {
        VlorQLError::validation(kind, details)
    }
}

/// A convenient constructor namespace for policy errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyError;

/// A convenient constructor namespace for compilation errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompilationError;

/// A convenient constructor namespace for schema errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct SchemaError;

/// A convenient constructor namespace for LLM errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct LlmError;

/// A convenient constructor namespace for configuration errors.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigError;

/// A collection of validation errors returned after validating a whole request.
///
/// Use [`ValidationErrors::validate`] to convert an iterator of
/// [`VlorQLError`] into `Result<(), Self>`. The vector can be
/// inspected via [`ValidationErrors::as_slice`].
///
/// # Examples
///
/// ```
/// use vlorql_core::errors::{VlorQLError, ValidationErrorKind, ValidationErrors};
/// use serde_json::json;
///
/// let errors = ValidationErrors::new(vec![
///     VlorQLError::validation(
///         ValidationErrorKind::InvalidJson,
///         json!({"response": "bad"}),
///     ),
/// ]);
/// assert_eq!(errors.len(), 1);
/// assert!(!errors.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationErrors(
    /// The individual errors collected by the validation pipeline.
    pub Vec<VlorQLError>,
);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.len() {
            0 => formatter.write_str("no validation errors"),
            1 => {
                let error = &self.0[0];
                write!(
                    formatter,
                    "{} validation error: {}",
                    error.error_code(),
                    error
                )
            }
            count => {
                writeln!(
                    formatter,
                    "{count} validation errors occurred (codes: {}):",
                    self.0
                        .iter()
                        .map(VlorQLError::error_code)
                        .collect::<Vec<_>>()
                        .join(", ")
                )?;
                for (index, error) in self.0.iter().enumerate() {
                    writeln!(formatter, "  [{index}] {}: {error}", error.error_code())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ValidationErrors {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl ValidationErrors {
    /// Creates an error collection from an iterator.
    pub fn new<I>(errors: I) -> Self
    where
        I: IntoIterator<Item = VlorQLError>,
    {
        Self(errors.into_iter().collect())
    }

    /// Validates a collection of errors, returning `Ok(())` when there are none.
    pub fn validate<I>(errors: I) -> Result<(), Self>
    where
        I: IntoIterator<Item = VlorQLError>,
    {
        let errors = Self::new(errors);
        if errors.0.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Returns the number of collected errors.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether no validation errors were collected.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the collected errors as a slice.
    pub fn as_slice(&self) -> &[VlorQLError] {
        &self.0
    }

    /// Consumes the collection and returns its errors.
    pub fn into_inner(self) -> Vec<VlorQLError> {
        self.0
    }
}

impl From<Vec<VlorQLError>> for ValidationErrors {
    fn from(errors: Vec<VlorQLError>) -> Self {
        Self(errors)
    }
}

/// Returns `Ok(())` when the iterator contains no errors, or aggregates all errors.
pub fn validate<I>(errors: I) -> Result<(), ValidationErrors>
where
    I: IntoIterator<Item = VlorQLError>,
{
    ValidationErrors::validate(errors)
}

fn details_to_value<T: Serialize>(details: T) -> Value {
    serde_json::to_value(details).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string(),
        })
    })
}

fn available_values_suggestion(prefix: String, values: &[String]) -> Option<String> {
    if values.is_empty() {
        Some(format!("{prefix}; no alternatives are available."))
    } else {
        Some(format!("{prefix}: {}.", values.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::error::Error;

    #[test]
    fn validation_error_serializes_with_context() {
        let error = VlorQLError::validation(
            ValidationErrorKind::InvalidColumn {
                table: "users".to_owned(),
                column: "emali".to_owned(),
                available_columns: vec!["email".to_owned(), "id".to_owned()],
            },
            json!({"path": ["select", 0]}),
        );

        let serialized = serde_json::to_value(&error).expect("error should serialize");
        assert_eq!(
            serialized["Validation"]["kind"]["invalid_column"]["table"],
            "users"
        );
        assert_eq!(serialized["Validation"]["details"]["path"][0], "select");
    }

    #[test]
    fn error_code_and_response_include_repair_guidance() {
        let error = VlorQLError::validation(
            ValidationErrorKind::InvalidTable {
                table: "usrers".to_owned(),
                available_tables: vec!["users".to_owned()],
            },
            json!({"source": "llm"}),
        );

        let response = error.to_error_response();
        assert_eq!(error.error_code(), "V003");
        assert_eq!(response.code, "V003");
        assert!(response.message.contains("usrers"));
        assert_eq!(response.details["source"], "llm");
        assert!(
            response
                .suggestion
                .as_deref()
                .is_some_and(|suggestion| suggestion.contains("users"))
        );
    }

    #[test]
    fn all_error_categories_have_unique_codes() {
        let errors = [
            VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
            VlorQLError::policy(
                PolicyErrorKind::TableDenied {
                    table: "secrets".to_owned(),
                },
                json!({}),
            ),
            VlorQLError::compilation(
                CompilationErrorKind::InvalidPlaceholder { index: 0 },
                json!({}),
            ),
            VlorQLError::schema(
                SchemaErrorKind::TableNotFound {
                    table: "users".to_owned(),
                },
                json!({}),
            ),
            VlorQLError::llm(LlmErrorKind::Timeout, json!({})),
            VlorQLError::config(ConfigErrorKind::MissingSchema, json!({})),
        ];
        let codes: std::collections::HashSet<_> =
            errors.iter().map(VlorQLError::error_code).collect();
        assert_eq!(codes.len(), errors.len());
    }

    #[test]
    fn retryability_distinguishes_recoverable_and_configuration_errors() {
        let validation = VlorQLError::validation(
            ValidationErrorKind::MissingField {
                field: "from".to_owned(),
            },
            json!({}),
        );
        let llm = VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: "bad JSON".to_owned(),
            },
            json!({}),
        );
        let policy = VlorQLError::policy(
            PolicyErrorKind::ColumnDenied {
                table: "users".to_owned(),
                column: "password_hash".to_owned(),
            },
            json!({}),
        );
        let config = VlorQLError::config(
            ConfigErrorKind::InvalidDialect {
                dialect: "unknown".to_owned(),
            },
            json!({}),
        );

        assert!(validation.is_retryable());
        assert!(llm.is_retryable());
        assert!(!policy.is_retryable());
        assert!(!config.is_retryable());
    }

    #[test]
    fn validation_errors_aggregate_and_implement_error() {
        let errors = vec![
            VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
            VlorQLError::validation(
                ValidationErrorKind::MissingField {
                    field: "select".to_owned(),
                },
                json!({}),
            ),
        ];
        let aggregated = ValidationErrors::from(errors);

        assert_eq!(aggregated.len(), 2);
        assert!(!aggregated.is_empty());
        let rendered = aggregated.to_string();
        assert!(rendered.contains("2 validation errors occurred"));
        assert!(rendered.contains("V001"));
        assert!(rendered.contains("V002"));
        assert!(rendered.contains("[0]"));
        assert!(rendered.contains("[1]"));
        assert!(rendered.contains("the LLM response is not valid JSON"));
        assert!(rendered.contains("required field `select` is missing"));
        assert!(aggregated.source().is_none());
        assert!(ValidationErrors::validate(Vec::<VlorQLError>::new()).is_ok());
        assert!(ValidationErrors::validate(aggregated.as_slice().iter().cloned()).is_err());
    }

    // -----------------------------------------------------------------
    // Exhaustive coverage of every error kind, asserting that
    // (a) construction works, (b) error_code() returns the expected
    // string, and (c) to_error_response() exposes the right fields.
    // -----------------------------------------------------------------

    fn assert_code_and_suggestion(
        error: &VlorQLError,
        expected_code: &str,
        expected_suggestion: Option<&str>,
    ) {
        assert_eq!(error.error_code(), expected_code, "code for {error:?}");
        let response = error.to_error_response();
        assert_eq!(response.code, expected_code, "response code for {error:?}");
        assert!(!response.message.is_empty(), "message for {error:?}");
        match expected_suggestion {
            Some(suggestion) => assert_eq!(
                response.suggestion.as_deref(),
                Some(suggestion),
                "suggestion for {error:?}"
            ),
            None => assert!(
                response.suggestion.is_none(),
                "expected no suggestion for {error:?}, got {:?}",
                response.suggestion
            ),
        }
        // The details payload must round-trip through serde.
        serde_json::to_value(&response).expect("response should serialize");
    }

    #[test]
    fn every_validation_error_kind_has_a_distinct_code_and_useful_suggestion() {
        assert_code_and_suggestion(
            &VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
            "V001",
            Some("Return a JSON object matching the query plan schema."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::MissingField {
                    field: "from".to_owned(),
                },
                json!({}),
            ),
            "V002",
            Some("Add the required field `from` to the query plan."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::InvalidColumn {
                    table: "users".to_owned(),
                    column: "emali".to_owned(),
                    available_columns: vec!["email".to_owned(), "id".to_owned()],
                },
                json!({}),
            ),
            "V004",
            Some("Replace column `users.emali` with an available column: email, id."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::InvalidFunction {
                    function: "load_extension".to_owned(),
                    allowed_functions: vec!["count".to_owned()],
                },
                json!({}),
            ),
            "V005",
            Some("Replace function `load_extension` with an allowed function: count."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::TypeMismatch {
                    expected: "int".to_owned(),
                    found: "string".to_owned(),
                    expr: "users.id".to_owned(),
                },
                json!({}),
            ),
            "V006",
            Some("Change `users.id` from type `string` to the expected type `int`."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::DialectFeatureDisabled {
                    feature: "cte".to_owned(),
                },
                json!({}),
            ),
            "V007",
            Some("Remove `cte` or select a dialect profile that enables it."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::TooManyJoins { actual: 5, max: 2 },
                json!({}),
            ),
            "V008",
            Some("Reduce the query from 5 joins to at most 2 joins."),
        );
        assert_code_and_suggestion(
            &VlorQLError::validation(
                ValidationErrorKind::AggregationMismatch {
                    message: "mixed aggregation".to_owned(),
                },
                json!({}),
            ),
            "V009",
            Some(
                "Adjust the selected and grouped expressions to satisfy aggregation rules: \
                 mixed aggregation",
            ),
        );

        // `InvalidTable` with no available alternatives gets a slightly
        // different suggestion.
        let isolated = VlorQLError::validation(
            ValidationErrorKind::InvalidTable {
                table: "missing".to_owned(),
                available_tables: vec![],
            },
            json!({}),
        );
        let response = isolated.to_error_response();
        assert_eq!(response.code, "V003");
        assert!(
            response
                .suggestion
                .as_deref()
                .is_some_and(|s| s.contains("no alternatives")),
            "got {:?}",
            response.suggestion
        );
    }

    #[test]
    fn every_policy_error_kind_has_a_distinct_code() {
        let shared_suggestion = "Request the required access or remove the unauthorized resource.";
        let table = VlorQLError::policy(
            PolicyErrorKind::TableDenied {
                table: "secrets".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(&table, "P001", Some(shared_suggestion));

        let column = VlorQLError::policy(
            PolicyErrorKind::ColumnDenied {
                table: "users".to_owned(),
                column: "password_hash".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(&column, "P002", Some(shared_suggestion));
    }

    #[test]
    fn policy_errors_share_a_default_suggestion() {
        let policy = VlorQLError::policy(
            PolicyErrorKind::ColumnDenied {
                table: "users".to_owned(),
                column: "secret".to_owned(),
            },
            json!({}),
        );
        let response = policy.to_error_response();
        assert_eq!(response.code, "P002");
        assert_eq!(
            response.suggestion.as_deref(),
            Some("Request the required access or remove the unauthorized resource.")
        );
    }

    #[test]
    fn every_compilation_error_kind_has_a_distinct_code() {
        let shared_suggestion = "Use only features supported by the selected SQL dialect compiler.";
        let unsupported = VlorQLError::compilation(
            CompilationErrorKind::UnsupportedDialectFeature {
                feature: "FETCH".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(&unsupported, "C001", Some(shared_suggestion));

        let placeholder = VlorQLError::compilation(
            CompilationErrorKind::InvalidPlaceholder { index: 7 },
            json!({}),
        );
        assert_code_and_suggestion(&placeholder, "C002", Some(shared_suggestion));
    }

    #[test]
    fn every_schema_error_kind_has_a_distinct_code() {
        let missing_table = VlorQLError::schema(
            SchemaErrorKind::TableNotFound {
                table: "ghost".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(
            &missing_table,
            "S001",
            Some(
                "Add the table as a JOIN (with an ON clause) or as the FROM source. If you reference columns with 'table: \"<name>\"', that table must be in FROM or JOINs.",
            ),
        );

        let missing_column = VlorQLError::schema(
            SchemaErrorKind::ColumnNotFound {
                table: "users".to_owned(),
                column: "secret".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(
            &missing_column,
            "S002",
            Some("Use only column names listed in the schema for the referenced table."),
        );

        let not_in_scope = VlorQLError::schema(
            SchemaErrorKind::TableNotInScope {
                table: "users".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(
            &not_in_scope,
            "S003",
            Some(
                "The table exists in the schema but is not part of the query's FROM or JOIN clauses. Add a JOIN (with an ON clause) for this table.",
            ),
        );
    }

    #[test]
    fn every_llm_error_kind_has_a_distinct_code_and_suggestion() {
        let transient = VlorQLError::llm(
            LlmErrorKind::ApiError {
                status: 500,
                message: "down".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(
            &transient,
            "L001",
            Some("Retry the LLM request with backoff, then inspect the provider status."),
        );

        let timeout = VlorQLError::llm(LlmErrorKind::Timeout, json!({}));
        assert_code_and_suggestion(
            &timeout,
            "L002",
            Some("Retry the LLM request or increase the request timeout."),
        );

        let parse = VlorQLError::llm(
            LlmErrorKind::ParseError {
                details: "bad JSON".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(
            &parse,
            "L003",
            Some(
                "Return only a JSON object matching the QueryPlan schema. No markdown fences, no extra text, no comments.",
            ),
        );
    }

    #[test]
    fn llm_suggestion_distinguishes_auth_failures_from_transient_errors() {
        let auth = VlorQLError::llm(
            LlmErrorKind::ApiError {
                status: 401,
                message: "unauthorized".to_owned(),
            },
            json!({}),
        );
        let transient = VlorQLError::llm(
            LlmErrorKind::ApiError {
                status: 503,
                message: "down".to_owned(),
            },
            json!({}),
        );
        assert_eq!(
            auth.to_error_response().suggestion.as_deref(),
            Some("Check the LLM provider credentials and permissions.")
        );
        assert_eq!(
            transient.to_error_response().suggestion.as_deref(),
            Some("Retry the LLM request with backoff, then inspect the provider status.")
        );
    }

    #[test]
    fn every_config_error_kind_has_a_distinct_code_and_no_suggestion() {
        let missing_client = VlorQLError::config(ConfigErrorKind::MissingLlmClient, json!({}));
        assert_code_and_suggestion(&missing_client, "G001", None);

        let missing_schema = VlorQLError::config(ConfigErrorKind::MissingSchema, json!({}));
        assert_code_and_suggestion(&missing_schema, "G002", None);

        let bad_dialect = VlorQLError::config(
            ConfigErrorKind::InvalidDialect {
                dialect: "oracle".to_owned(),
            },
            json!({}),
        );
        assert_code_and_suggestion(&bad_dialect, "G003", None);
    }

    #[test]
    fn only_validation_and_llm_errors_are_retryable() {
        let cases = [
            (
                VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
                true,
            ),
            (
                VlorQLError::validation(
                    ValidationErrorKind::TooManyJoins { actual: 3, max: 1 },
                    json!({}),
                ),
                false,
            ),
            (VlorQLError::llm(LlmErrorKind::Timeout, json!({})), true),
            (
                VlorQLError::llm(
                    LlmErrorKind::ApiError {
                        status: 500,
                        message: "down".to_owned(),
                    },
                    json!({}),
                ),
                true,
            ),
            (
                VlorQLError::policy(
                    PolicyErrorKind::TableDenied {
                        table: "secrets".to_owned(),
                    },
                    json!({}),
                ),
                false,
            ),
            (
                VlorQLError::policy(
                    PolicyErrorKind::ColumnDenied {
                        table: "users".to_owned(),
                        column: "password_hash".to_owned(),
                    },
                    json!({}),
                ),
                false,
            ),
            (
                VlorQLError::compilation(
                    CompilationErrorKind::UnsupportedDialectFeature {
                        feature: "FETCH".to_owned(),
                    },
                    json!({}),
                ),
                false,
            ),
            (
                VlorQLError::schema(
                    SchemaErrorKind::TableNotFound {
                        table: "missing".to_owned(),
                    },
                    json!({}),
                ),
                true,
            ),
            (
                VlorQLError::schema(
                    SchemaErrorKind::TableNotInScope {
                        table: "users".to_owned(),
                    },
                    json!({}),
                ),
                true,
            ),
            (
                VlorQLError::config(ConfigErrorKind::MissingSchema, json!({})),
                false,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                error.is_retryable(),
                expected,
                "retryability for {:?} should be {expected}",
                error
            );
        }
    }

    #[test]
    fn error_response_round_trips_through_serde() {
        let error = VlorQLError::validation(
            ValidationErrorKind::TypeMismatch {
                expected: "int".to_owned(),
                found: "string".to_owned(),
                expr: "users.id".to_owned(),
            },
            json!({"path": ["where"]}),
        );
        let response = error.to_error_response();
        let serialized =
            serde_json::to_string(&response).expect("response should serialize to JSON");
        let restored: ErrorResponse =
            serde_json::from_str(&serialized).expect("response should round-trip");
        assert_eq!(restored.code, response.code);
        assert_eq!(restored.message, response.message);
        assert_eq!(restored.suggestion, response.suggestion);
        assert_eq!(restored.details, response.details);
    }

    #[test]
    fn kind_name_reports_the_top_level_category() {
        let validation = VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({}));
        let policy = VlorQLError::policy(
            PolicyErrorKind::TableDenied {
                table: "x".to_owned(),
            },
            json!({}),
        );
        let compilation = VlorQLError::compilation(
            CompilationErrorKind::InvalidPlaceholder { index: 1 },
            json!({}),
        );
        let schema = VlorQLError::schema(
            SchemaErrorKind::TableNotFound {
                table: "x".to_owned(),
            },
            json!({}),
        );
        let llm = VlorQLError::llm(LlmErrorKind::Timeout, json!({}));
        let config = VlorQLError::config(ConfigErrorKind::MissingSchema, json!({}));
        assert_eq!(validation.kind_name(), "validation");
        assert_eq!(policy.kind_name(), "policy");
        assert_eq!(compilation.kind_name(), "compilation");
        assert_eq!(schema.kind_name(), "schema");
        assert_eq!(llm.kind_name(), "llm");
        assert_eq!(config.kind_name(), "config");
    }

    #[test]
    fn single_validation_error_renders_without_count_header() {
        let error = VlorQLError::validation(
            ValidationErrorKind::MissingField {
                field: "from".to_owned(),
            },
            json!({}),
        );
        let aggregated = ValidationErrors::from(vec![error]);
        let rendered = aggregated.to_string();
        assert!(
            rendered.contains("1 validation error:") || rendered.contains("validation error:"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("V002"), "rendered: {rendered}");
        assert!(
            rendered.contains("required field `from` is missing"),
            "rendered: {rendered}"
        );
        // Single-error rendering should not contain the multi-error
        // "occurred (codes:" line.
        assert!(
            !rendered.contains("occurred (codes:"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn empty_validation_errors_renders_no_errors_message() {
        let aggregated = ValidationErrors(Vec::new());
        assert_eq!(aggregated.to_string(), "no validation errors");
    }

    #[test]
    fn validation_errors_into_inner_returns_owned_vector() {
        let errors = vec![VlorQLError::validation(
            ValidationErrorKind::InvalidJson,
            json!({}),
        )];
        let aggregated = ValidationErrors::from(errors);
        let inner = aggregated.into_inner();
        assert_eq!(inner.len(), 1);
    }

    #[test]
    fn free_function_validate_aggregates_iterator_input() {
        let errors = [
            VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
            VlorQLError::validation(
                ValidationErrorKind::MissingField {
                    field: "select".to_owned(),
                },
                json!({}),
            ),
        ];
        let result = crate::errors::validate(errors);
        let aggregated = result.expect_err("non-empty iterator should produce ValidationErrors");
        assert_eq!(aggregated.len(), 2);
    }

    #[test]
    fn validation_errors_serialize_and_deserialize_through_serde() {
        let errors = vec![
            VlorQLError::validation(ValidationErrorKind::InvalidJson, json!({})),
            VlorQLError::policy(
                PolicyErrorKind::TableDenied {
                    table: "secrets".to_owned(),
                },
                json!({}),
            ),
        ];
        let aggregated = ValidationErrors::from(errors);
        let serialized = serde_json::to_string(&aggregated).expect("should serialize");
        let restored: ValidationErrors =
            serde_json::from_str(&serialized).expect("should round-trip");
        assert_eq!(restored, aggregated);
    }
}
