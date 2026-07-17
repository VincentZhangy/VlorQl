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

pub mod compile;
pub mod errors;
pub mod policy;
pub mod prompt;
pub(crate) mod query;
pub mod schema;
pub mod statistics;
pub mod validate;
