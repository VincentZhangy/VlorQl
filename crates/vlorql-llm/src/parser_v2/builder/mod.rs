//! Builder layer: canonical JSON → [`QueryPlan`] AST.
//!
//! This layer consumes the canonical JSON produced by the
//! [normalize](crate::parser_v2::normalize) layer and produces a
//! typed [`QueryPlan`].  It does **no** repair — it assumes the input
//! has already been normalized.
//!
//! # Sub-modules
//!
//! - **expr_builder** — Expression and Predicate building, error types,
//!   operator/type parsers, field extraction helpers
//! - **table_builder** — FromClause building
//! - **select_builder** — Projection / SELECT building
//! - **join_builder** — JoinClause building
//! - **query_builder** — QueryPlan building (orchestrator)

pub mod expr_builder;
pub mod join_builder;
pub mod query_builder;
pub mod select_builder;
pub mod table_builder;

pub use query_builder::{build_plan, from_canonical_str, from_canonical_value};