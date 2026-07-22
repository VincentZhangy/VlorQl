//! Builder for [`FunctionDef`].
//!
//! Provides a chainable API that is more readable than constructing
//! the struct directly.

use std::borrow::Cow;

use crate::schema::DataType;

use super::def::{Dialect, FunctionDef, FunctionKind};

/// Builder for [`FunctionDef`].
#[derive(Debug, Clone)]
pub struct FunctionDefBuilder {
    names: Vec<Cow<'static, str>>,
    kind: FunctionKind,
    min_args: usize,
    max_args: Option<usize>,
    param_types: Option<Vec<Option<DataType>>>,
    return_type: Option<DataType>,
    supports_distinct: bool,
    supports_order_by: bool,
    allows_star: bool,
    dialects: Vec<Dialect>,
}

impl FunctionDefBuilder {
    /// Start building a function with the given canonical name.
    pub fn new(name: &'static str) -> Self {
        Self {
            names: vec![Cow::Borrowed(name)],
            kind: FunctionKind::Scalar,
            min_args: 0,
            max_args: None,
            param_types: None,
            return_type: None,
            supports_distinct: false,
            supports_order_by: false,
            allows_star: false,
            dialects: Vec::new(),
        }
    }

    /// Add an alias for this function.
    pub fn alias(mut self, alias: &'static str) -> Self {
        self.names.push(Cow::Borrowed(alias));
        self
    }

    /// Set the function category.
    pub fn kind(mut self, kind: FunctionKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set the minimum number of arguments.
    pub fn min_args(mut self, min: usize) -> Self {
        self.min_args = min;
        self
    }

    /// Set the maximum number of arguments (`None` = unlimited).
    pub fn max_args(mut self, max: Option<usize>) -> Self {
        self.max_args = max;
        self
    }

    /// Set expected parameter types for type-checking.
    pub fn param_types(mut self, types: &[Option<DataType>]) -> Self {
        self.param_types = Some(types.to_vec());
        self
    }

    /// Set the return type for type inference.
    pub fn return_type(mut self, ty: DataType) -> Self {
        self.return_type = Some(ty);
        self
    }

    /// Whether `DISTINCT` is allowed (e.g. `COUNT(DISTINCT col)`).
    pub fn supports_distinct(mut self, yes: bool) -> Self {
        self.supports_distinct = yes;
        self
    }

    /// Whether an `ORDER BY` child clause is allowed.
    pub fn supports_order_by(mut self, yes: bool) -> Self {
        self.supports_order_by = yes;
        self
    }

    /// Whether `*` is accepted as an argument (e.g. `COUNT(*)`).
    pub fn allows_star(mut self, yes: bool) -> Self {
        self.allows_star = yes;
        self
    }

    /// Restrict this function to specific SQL dialects.
    pub fn dialects(mut self, dialects: &[Dialect]) -> Self {
        self.dialects = dialects.to_vec();
        self
    }

    /// Finalise the builder and produce a [`FunctionDef`].
    pub fn build(self) -> FunctionDef {
        FunctionDef {
            names: self.names,
            kind: self.kind,
            min_args: self.min_args,
            max_args: self.max_args,
            param_types: self.param_types,
            return_type: self.return_type,
            supports_distinct: self.supports_distinct,
            supports_order_by: self.supports_order_by,
            allows_star: self.allows_star,
            dialects: self.dialects,
        }
    }
}
