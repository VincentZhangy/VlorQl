//! Core data types, validators, and SQL compilation for VlorQl.
//!
//! This crate implements the *plan-then-execute* boundary described
//! in the VlorQl design notes:
//!
//! * The LLM only ever emits a [`QueryPlan`](schema::QueryPlan) (a typed JSON object).
//! * [`validate::ValidationPipeline`] runs schema, policy, operand,
//!   and dialect validation on the plan and aggregates every
//!   violation into a single [`errors::ValidationErrors`].
//! * [`compile::QueryBuilder`] turns a validated plan into
//!   parameterized SQL, delegating dialect-specific syntax to a
//!   [`compile::SqlCompiler`] implementation.
//!
//! The crate has no I/O dependencies and can be used standalone
//! (e.g. to construct or inspect plans in unit tests) or as the
//! foundation of the higher-level [`vlorql`](https://docs.rs/vlorql)
//! facade.

#![deny(missing_docs)]

pub mod cache;
pub mod compile;
pub mod errors;
pub(crate) mod fix;
pub mod function;
pub mod observability;
pub mod optimizer;
pub mod policy;
pub mod prompt;
pub(crate) mod query;
pub mod schema;
pub mod statistics;
pub mod validate;

/// Initialise the function registry with built-in functions.
///
/// Should be called once during program startup (e.g. from the
/// application's `main` or the `vlorql` facade's `build` method).
pub fn init_function_registry() {
    function::init_registry(function::builtin::builtin_functions());
}
