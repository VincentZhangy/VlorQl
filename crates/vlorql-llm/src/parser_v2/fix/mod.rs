//! Auto Fix layer: opinionated, safe fixes for [`QueryPlan`] AST.
//!
//! This layer runs after the [builder](crate::parser_v2::builder) and
//! before the [validator](crate::parser_v2::validate).  It applies
//! universally safe defaults to fix common LLM output issues.
//!
//! # Sub-modules
//!
//! - **fixer** — Auto Fix Engine implementation

pub mod fixer;

pub use fixer::{apply_fixes, fix_plan};