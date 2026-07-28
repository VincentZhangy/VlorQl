//! V2 Parsing Pipeline — LLM output → QueryPlan.
//!
//! A staged, layered pipeline designed for multi-model compatibility.
//! Each stage is an independent module with a clear responsibility:
//!
//! | Stage | Module | Input → Output |
//! |-------|--------|---------------|
//! | 1. Recover | [`recover`] | Raw LLM text → JSON string |
//! | 2. Normalize | [`normalize`] | Messy JSON → canonical JSON |
//! | 3. Build | [`builder`] | Canonical JSON → `QueryPlan` AST |
//! | 4. Fix | [`fix`] | Auto-fix engine (missing aliases, limit zero, etc.) |
//! | 5. Validate | [`validate`] | Semantic validation of the plan |
//! | 6. Optimize | [`optimize`] | AST-level optimizations |
//!
//! ## Pipeline Orchestration
//!
//! The [`pipeline`] module ties all stages together. The recommended
//! entry points are:
//!
//! - [`pipeline::parse_query_plan`] — standard pipeline
//! - [`pipeline::parse_query_plan_lenient`] — tolerant pipeline (small models)
//! - [`pipeline::parse_query_plan_debug`] — debug pipeline with detailed results
//!
//! ## Data Flow
//!
//! ```text
//! LLM text
//!    │
//!    ▼
//! [recover]  ─── raw text → JSON string
//!    │
//!    ▼
//! [normalize] ── messy JSON → canonical field names & structures
//!    │                  └── model-specific pipeline for small models
//!    ▼
//! [builder]  ─── canonical JSON → typed QueryPlan AST
//!    │
//!    ▼
//! [fix]      ─── auto-repair common LLM mistakes
//!    │
//!    ▼
//! [validate] ─── semantic checks (schema, dialect, operand types)
//!    │
//!    ▼
//! [optimize] ─── predicate simplification, projection pruning
//!    │
//!    ▼
//! QueryPlan
//! ```
//!
//! ## Error Handling
//!
//! Each stage produces structured errors collected in
//! [`pipeline::ParseError`]. The pipeline does not stop at the first
//! error — it collects all errors from all stages so callers can display
//! a complete picture.
//!
//! ## Multi-Model Support
//!
//! The normalize stage detects the model fingerprint and applies
//! model-specific normalizations (e.g., for `llama-3.2`, `qwen2.5`,
//! `phi-3`, `deepseek-coder`). The small-model pipeline adds extra
//! fixes for common patterns like `"from": "table_name"` (string)
//! instead of `"from": {"table": "table_name"}` (object).
//!
//! The recommended entry point is [`pipeline::parse_query_plan`].

pub mod builder;
pub mod fix;
pub mod normalize;
pub mod optimize;
pub mod pipeline;
pub mod recover;
pub mod validate;
