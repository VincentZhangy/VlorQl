//! The [`VlorQLError`] enum and its constructor / method impls.

//! The [`VlorQLError`] enum, constructor methods, and method impls.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use super::ErrorResponse;
use super::kinds::{
    AuditErrorKind, CompilationErrorKind, ConfigErrorKind, LlmErrorKind, PolicyErrorKind,
    SchemaErrorKind, ValidationErrorKind,
};

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
    /// The query plan failed the SQL-injection audit.
    #[error("audit error: {kind}")]
    Audit {
        /// The typed audit failure.
        kind: AuditErrorKind,
        /// Structured context (identifier, pattern, …).
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

    /// Creates an audit error from any serializable details value.
    pub fn audit<T: Serialize>(kind: AuditErrorKind, details: T) -> Self {
        Self::Audit {
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
            Self::Audit { kind, .. } => match kind {
                AuditErrorKind::IdentifierNotFound { .. } => "A001",
                AuditErrorKind::SuspiciousPattern { .. } => "A002",
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
                ConfigErrorKind::InternalError { .. } => "G007",
            },
        }
    }

    /// Converts this error into a machine-readable response with repair guidance.
    pub fn to_error_response(&self) -> ErrorResponse {
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
            Self::Audit { kind, .. } => matches!(kind, AuditErrorKind::IdentifierNotFound { .. }),
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
            | Self::Audit { details, .. }
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
            Self::Audit { .. } => "audit",
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
            Self::Audit { kind, .. } => match kind {
                AuditErrorKind::IdentifierNotFound { .. } => {
                    Some("Use a valid table or column name from the schema snapshot.".to_owned())
                }
                AuditErrorKind::SuspiciousPattern { .. } => {
                    Some("Remove SQL-injection patterns from identifiers.".to_owned())
                }
            },
            Self::Config { kind, .. } => match kind {
                ConfigErrorKind::MissingLlmClient => {
                    Some("Configure an LLM client via VlorQlBuilder::with_llm_client or provide an API key.".to_owned())
                }
                ConfigErrorKind::MissingSchema => {
                    Some("Provide a schema snapshot via VlorQlBuilder::with_schema.".to_owned())
                }
                ConfigErrorKind::InvalidDialect { dialect } => Some(format!(
                    "`{dialect}` is not a recognized dialect. Use one of: postgres, mysql, sqlite."
                )),
                ConfigErrorKind::MissingApiKey { provider } => Some(format!(
                    "Set the API key for {provider} via the API_KEY environment variable or VlorQlBuilder."
                )),
                ConfigErrorKind::EmptyModel => {
                    Some("Set a non-empty model name via VlorQlBuilder::with_model.".to_owned())
                }
                ConfigErrorKind::ConfigFileError { path, .. } => Some(format!(
                    "Check that `{path}` exists, is readable, and contains valid configuration."
                )),
                ConfigErrorKind::InternalError { .. } => {
                    Some("This is an internal error — check logs and retry. If it persists, file a bug report.".to_owned())
                }
            },
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

pub(crate) fn details_to_value<T: Serialize>(details: T) -> Value {
    serde_json::to_value(details).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string(),
        })
    })
}

pub(crate) fn available_values_suggestion(prefix: String, values: &[String]) -> Option<String> {
    if values.is_empty() {
        Some(format!("{prefix}; no alternatives are available."))
    } else {
        Some(format!("{prefix}: {}.", values.join(", ")))
    }
}
