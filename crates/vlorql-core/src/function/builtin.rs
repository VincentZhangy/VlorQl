//! Built-in function definitions.
//!
//! Returns the default set of functions that are loaded into the
//! registry at startup.  Add new functions here to make them available
//! to all downstream layers.

use crate::schema::DataType;

use super::def::{Dialect, FunctionKind};
use super::builder::FunctionDefBuilder;
use super::def::FunctionDef;

/// All built-in functions.
pub fn builtin_functions() -> Vec<FunctionDef> {
    vec![
        // ── Aggregate functions ──────────────────────────────────
        FunctionDefBuilder::new("sum")
            .kind(FunctionKind::Aggregate)
            .min_args(1)
            .max_args(Some(1))
            .param_types(&[Some(DataType::Float)])
            .return_type(DataType::Float)
            .supports_distinct(true)
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("count")
            .kind(FunctionKind::Aggregate)
            .min_args(0)
            .max_args(Some(1))
            .param_types(&[Some(DataType::Int)])
            .return_type(DataType::Int)
            .supports_distinct(true)
            .allows_star(true)
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("avg")
            .kind(FunctionKind::Aggregate)
            .min_args(1)
            .max_args(Some(1))
            .param_types(&[Some(DataType::Float)])
            .return_type(DataType::Float)
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("min")
            .kind(FunctionKind::Aggregate)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("max")
            .kind(FunctionKind::Aggregate)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("string_agg")
            .kind(FunctionKind::Aggregate)
            .min_args(2)
            .max_args(Some(2))
            .supports_order_by(true)
            .dialects(&[Dialect::Postgres])
            .build(),
        FunctionDefBuilder::new("array_agg")
            .kind(FunctionKind::Aggregate)
            .min_args(1)
            .max_args(Some(1))
            .supports_order_by(true)
            .dialects(&[Dialect::Postgres])
            .build(),
        // ── Scalar functions ─────────────────────────────────────
        FunctionDefBuilder::new("upper")
            .alias("ucase")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("lower")
            .alias("lcase")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("length")
            .alias("len")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("substr")
            .alias("substring")
            .kind(FunctionKind::Scalar)
            .min_args(2)
            .max_args(Some(3))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("coalesce")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(None)
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("abs")
            .alias("absval")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(Some(1))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("round")
            .kind(FunctionKind::Scalar)
            .min_args(1)
            .max_args(Some(2))
            .dialects(&[Dialect::Generic])
            .build(),
        // ── Window functions ─────────────────────────────────────
        FunctionDefBuilder::new("row_number")
            .kind(FunctionKind::Window)
            .min_args(0)
            .max_args(Some(0))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("rank")
            .kind(FunctionKind::Window)
            .min_args(0)
            .max_args(Some(0))
            .dialects(&[Dialect::Generic])
            .build(),
        FunctionDefBuilder::new("dense_rank")
            .kind(FunctionKind::Window)
            .min_args(0)
            .max_args(Some(0))
            .dialects(&[Dialect::Generic])
            .build(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_contains_all_expected() {
        let funcs = builtin_functions();
        let names: Vec<&str> = funcs.iter().map(|f| f.canonical_name()).collect();
        for expected in &["sum", "count", "avg", "min", "max", "upper", "lower", "length", "substr", "coalesce", "abs", "round", "row_number", "rank", "dense_rank"] {
            assert!(names.contains(expected), "{expected} should be in builtin functions");
        }
    }

    #[test]
    fn string_agg_is_postgres_only() {
        let funcs = builtin_functions();
        let sa = funcs.iter().find(|f| f.canonical_name() == "string_agg").unwrap();
        assert!(sa.supports_dialect(Dialect::Postgres));
        assert!(!sa.supports_dialect(Dialect::Sqlite));
    }
}