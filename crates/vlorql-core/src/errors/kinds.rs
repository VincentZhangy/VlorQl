//! Structured error categories used by the VlorQl core.
//!
//! The variants intentionally carry the values that caused validation to fail. This
//! makes an error useful to both API consumers and an LLM that is asked to repair a
//! query plan.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors found while validating an LLM-generated query plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum ValidationErrorKind {
    /// The LLM response could not be decoded as JSON.
    #[error("the LLM response is not valid JSON")]
    InvalidJson,
    /// A required field was not present in the query plan.
    #[error("required field `{field}` is missing")]
    MissingField {
        /// Name of the missing field, e.g. `"from"`.
        field: String,
    },
    /// The query references a table that is not in the schema snapshot.
    #[error("table `{table}` is not available")]
    InvalidTable {
        /// Name of the offending table.
        table: String,
        /// Tables the operator can choose from.
        available_tables: Vec<String>,
    },
    /// The query references a column that is not in the selected table.
    #[error("column `{column}` is not available on table `{table}`")]
    InvalidColumn {
        /// Owning table of the offending column.
        table: String,
        /// Name of the offending column.
        column: String,
        /// Columns the operator can choose from.
        available_columns: Vec<String>,
    },
    /// The query uses a function outside the configured feature set.
    #[error("function `{function}` is not allowed")]
    InvalidFunction {
        /// Name of the offending function.
        function: String,
        /// Functions the operator is allowed to call.
        allowed_functions: Vec<String>,
    },
    /// An expression contains incompatible types.
    #[error("type mismatch in `{expr}`: expected `{expected}`, found `{found}`")]
    TypeMismatch {
        /// The expected SQL type, as a human-readable string.
        expected: String,
        /// The actual SQL type the validator found.
        found: String,
        /// Expression whose type is wrong (for repair messages).
        expr: String,
    },
    /// The query uses a feature disabled by the selected SQL dialect profile.
    #[error("dialect feature `{feature}` is disabled")]
    DialectFeatureDisabled {
        /// Identifier of the disabled feature (e.g. `"cte"`).
        feature: String,
    },
    /// The query exceeds the configured join limit.
    #[error("query contains {actual} joins, but the maximum is {max}")]
    TooManyJoins {
        /// Number of joins present in the plan.
        actual: usize,
        /// Configured maximum.
        max: usize,
    },
    /// Selected and grouped expressions do not satisfy aggregation rules.
    #[error("aggregation mismatch: {message}")]
    AggregationMismatch {
        /// Human-readable description of the rule that was violated.
        message: String,
    },
    /// Multiple validation errors occurred and could not be individually retried.
    #[error("query plan has {count} validation error(s)")]
    MultipleErrors {
        /// Number of distinct validation errors.
        count: usize,
    },
}

/// Errors raised when a query violates an access policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum PolicyErrorKind {
    /// The query references a table denied by policy.
    #[error("access to table `{table}` is denied")]
    TableDenied {
        /// Name of the denied table.
        table: String,
    },
    /// The query references a column denied by policy.
    #[error("access to column `{table}.{column}` is denied")]
    ColumnDenied {
        /// Owning table of the denied column.
        table: String,
        /// Name of the denied column.
        column: String,
    },
}

/// Errors raised while compiling a validated plan into SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum CompilationErrorKind {
    /// A validated plan still contains a feature unsupported by the compiler.
    #[error("dialect feature `{feature}` is not supported by the compiler")]
    UnsupportedDialectFeature {
        /// Identifier of the unsupported feature.
        feature: String,
    },
    /// A generated parameter placeholder is not valid for the selected dialect.
    #[error("parameter placeholder `{index}` is invalid")]
    InvalidPlaceholder {
        /// 1-based placeholder index that failed validation.
        index: usize,
    },
}

/// Errors caused by a schema that cannot satisfy a query plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum SchemaErrorKind {
    /// A referenced table does not exist in the schema snapshot.
    #[error("table `{table}` was not found in the schema")]
    TableNotFound {
        /// Name of the missing table.
        table: String,
    },
    /// A referenced table exists in the schema but is not part of the
    /// query's `FROM` or `JOIN` clauses.
    #[error(
        "table `{table}` exists in the schema but is not referenced in the FROM or JOIN clauses of the query"
    )]
    TableNotInScope {
        /// Name of the table that is missing from the query scope.
        table: String,
    },
    /// A referenced column does not exist on a table in the schema snapshot.
    #[error("column `{table}.{column}` was not found in the schema")]
    ColumnNotFound {
        /// Owning table of the missing column.
        table: String,
        /// Name of the missing column.
        column: String,
    },
}

/// Errors returned by an LLM provider or while decoding its response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    /// The provider returned an unsuccessful HTTP response.
    #[error("LLM API returned HTTP {status}: {message}")]
    ApiError {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Provider-supplied error message.
        message: String,
    },
    /// The provider did not respond before the configured deadline.
    #[error("LLM request timed out")]
    Timeout,
    /// The provider response could not be decoded into the requested plan.
    #[error("LLM response could not be parsed: {details}")]
    ParseError {
        /// Human-readable description of the parse failure.
        details: String,
    },
}

/// Errors caused by an incomplete or invalid VlorQl configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "snake_case")]
pub enum ConfigErrorKind {
    /// No LLM client was configured for an operation that requires one.
    #[error("an LLM client has not been configured")]
    MissingLlmClient,
    /// No schema snapshot was configured for an operation that requires one.
    #[error("a schema snapshot has not been configured")]
    MissingSchema,
    /// The configured SQL dialect is not recognized.
    #[error("SQL dialect `{dialect}` is invalid")]
    InvalidDialect {
        /// The unrecognized dialect identifier.
        dialect: String,
    },
    /// An API key is required but was not provided.
    #[error("API key is required for provider `{provider}`")]
    MissingApiKey {
        /// Name of the provider that requires a key.
        provider: String,
    },
    /// The model name is empty or whitespace-only.
    #[error("model name must not be empty")]
    EmptyModel,
    /// A statistics or configuration file could not be read or parsed.
    #[error("failed to load configuration file `{path}`: {reason}")]
    ConfigFileError {
        /// Filesystem path of the offending file.
        path: String,
        /// Human-readable description of the read or parse failure.
        reason: String,
    },
}
