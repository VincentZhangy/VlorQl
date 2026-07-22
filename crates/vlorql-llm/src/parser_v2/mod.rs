//! V2 Parsing Pipeline — LLM output → QueryPlan.
//!
//! A staged, layered pipeline designed for multi-model compatibility:
//!
//! 1. **recover** — raw text → JSON string (markdown, bracket, JSON extraction)
//! 2. **normalize** — messy JSON → canonical JSON (field aliases, structure, operators)
//! 3. **builder** — canonical JSON → QueryPlan AST (no repair)
//! 4. **fix** — auto-fix engine (safe defaults: missing aliases, limit zero, empty select)
//! 5. **validate** — semantic validation (non-empty SELECT, valid JOIN conditions, etc.)
//! 6. **optimize** — AST optimization (predicate simplification, projection pruning, rewrite)
//!
//! This module is built alongside the existing `parse` module and will
//! eventually replace it.
//!
//! The recommended entry point is [`pipeline::parse_query_plan`].

pub mod builder;
pub mod fix;
pub mod normalize;
pub mod optimize;
pub mod pipeline;
pub mod recover;
pub mod validate;