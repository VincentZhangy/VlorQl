//! Entry point for the `cargo test --test integration` binary.
//!
//! The actual integration tests live under `tests/integration/` and are
//! included here via explicit `#[path = "..."]` attributes so the
//! single binary can be run with `cargo test --test integration`.
//!
//! Submodules:
//!
//! * [`common`] — shared fixtures (schema, plan, policies, mock LLM
//!   clients). Every other submodule depends on this one.
//! * [`end_to_end`] — full natural-language → SQL pipeline tests.
//! * [`dialect_compilation`] — parameterized compilation tests across
//!   PostgreSQL, SQLite, and MySQL.
//! * [`policy_enforcement`] — table/column/row-filter policy checks.
//! * [`error_recovery`] — validation aggregation, retry classification,
//!   and short-circuiting tests.
//! * [`optimizer_tests`] — query-optimizer integration tests.
//! * [`cache_integration`] — cache layer integration tests.

#[path = "integration/common.rs"]
mod common;

#[path = "integration/end_to_end.rs"]
mod end_to_end;

#[path = "integration/dialect_compilation.rs"]
mod dialect_compilation;

#[path = "integration/policy_enforcement.rs"]
mod policy_enforcement;

#[path = "integration/error_recovery.rs"]
mod error_recovery;

#[path = "integration/optimizer_tests.rs"]
mod optimizer_tests;

#[path = "integration/cache_integration.rs"]
mod cache_integration;
