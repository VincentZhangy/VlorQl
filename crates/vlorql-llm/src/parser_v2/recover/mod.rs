//! Recover layer: raw LLM text → JSON string.
//!
//! Responsibilities:
//!
//! - **markdown** — strip fenced code blocks (`` ```json ``​, `` ``` ``​, etc.)
//! - **json** — JSON validity checks, array-to-object extraction
//! - **bracket** — brace/bracket matching with string-awareness
//! - **extract** — orchestration pipeline (`extract_json_content`)
//!
//! This layer produces a `&str` (a JSON string).  It does **not**
//! understand QueryPlan semantics.

pub mod bracket;
pub mod extract;
pub mod json;
pub mod markdown;

pub use extract::{detect_template_leak, extract_json_content};