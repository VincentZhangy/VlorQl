//! [`ValidationErrors`] collection, display, and the free-function `validate`.

use serde::{Deserialize, Serialize};

use super::vlorql_error::VlorQLError;

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
pub struct ValidationErrors(pub Vec<VlorQLError>);

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
