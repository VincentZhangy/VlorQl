//! Validate layer: semantic validation for [`QueryPlan`] AST.
//!
//! This layer validates the QueryPlan produced by the
//! [builder](crate::parser_v2::builder) layer before it is passed to
//! the SQL compiler.  It checks structural and semantic correctness
//! (non-empty SELECT, valid JOIN conditions, etc.) but does **not**
//! check against a schema snapshot or dialect profile.
//!
//! # Sub-modules
//!
//! - **validator** — ValidationError type, ValidationResult, entry point
//! - **semantic** — Semantic validation rules

pub mod semantic;
pub mod validator;

pub use validator::{ValidationError, ValidationErrorKind, ValidationResult, validate_plan};
