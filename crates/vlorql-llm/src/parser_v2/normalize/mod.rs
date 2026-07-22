//! Normalize layer: messy LLM JSON → canonical JSON.
//!
//! Responsibilities:
//!
//! - **aliases** — field-name alias table and normalization
//! - **operators** — (planned) operator name normalization
//! - **expr** — (planned) expression normalization
//! - **query** — (planned) query-level structure normalization
//! - **select** — (planned) SELECT clause normalization
//! - **where\_** — (planned) WHERE clause normalization
//! - **order** — (planned) ORDER BY clause normalization
//! - **join** — (planned) JOIN clause normalization
//! - **value** — (planned) value/literal normalization
//! - **array** — (planned) array normalization
//! - **table** — (planned) table clause normalization
//! - **common** — shared utilities
//! - **pipeline** — orchestration of all normalization stages
//!
//! This layer produces a `serde_json::Value` (canonical JSON).  It
//! does **not** understand QueryPlan semantics — that is the
//! builder's job.

pub mod aliases;
pub mod array;
pub mod common;
pub mod expr;
pub mod join;
pub mod operators;
pub mod order;
pub mod pipeline;
pub mod query;
pub mod select;
pub mod table;
pub mod value;
pub mod where_;

pub use pipeline::{normalize, normalize_for_model, normalize_str};
