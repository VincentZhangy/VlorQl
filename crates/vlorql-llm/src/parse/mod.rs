//! LLM output → [`QueryPlan`] parse pipeline.
//!
//! Layers (compiler-style):
//!
//! 1. **recover** — raw text → JSON text (`extract_json_content`)
//! 2. **canonicalize** — messy plan JSON → canonical JSON Value
//! 3. **build** — canonical JSON → typed [`vlorql_core::schema::QueryPlan`]
//!
//! Public crate APIs (`extract_json_content`, `repair_query_plan_json`,
//! `detect_template_leak`) are re-exported from the crate root for
//! compatibility.

pub mod build;
pub mod canonicalize;
pub mod recover;

pub use build::{from_canonical_str, from_canonical_value};
pub use canonicalize::{canonicalize_to_value, repair_query_plan_json};
pub use recover::{detect_template_leak, extract_json_content};

/// Full pipeline: recover is the caller's job (raw → JSON text); this runs
/// canonicalize + build.
///
/// # Errors
///
/// Returns a `serde_json::Error` when canonicalize cannot produce a value that
/// deserializes as [`vlorql_core::schema::QueryPlan`].
pub fn parse_query_plan(
    json_text: &str,
) -> Result<vlorql_core::schema::QueryPlan, serde_json::Error> {
    let repaired = repair_query_plan_json(json_text);
    from_canonical_str(&repaired)
}
