//! Schema-aware AST fixer that repairs common LLM mistakes before validation.
//!
//! These fixes are applied AFTER the builder produces a [`QueryPlan`] and BEFORE
//! [`validate::ValidationPipeline`] checks it.  Because they use the real schema
//! to look up tables, columns, and foreign-key relationships, they are more
//! reliable than heuristic JSON-level normalisation.

mod aggregates;
mod column_table;
mod columns;
pub(crate) mod group_by;
mod joins;

use crate::schema::{QueryPlan, SchemaSnapshot};
use tracing::debug;

/// Apply schema-aware fixes to `plan`.
///
/// Returns `true` if any change was made.
///
/// # Fixes applied (in order)
///
/// 0. **Arithmetic column refs** — column names like `"unit_price * quantity"`
///    are converted from a malformed `ColumnRef` into a proper `BinaryOp` tree.
/// 1. **Wrong-table column refs** — `order_items.name` rewritten to
///    `products.name` when `name` only exists on a FK-related table.
/// 2. **Missing joins** — tables referenced in SELECT/WHERE that are not in
///    FROM/JOINS get the correct INNER JOIN added, using the schema's actual
///    foreign-key metadata.
/// 3. **Nested aggregate dedup** — `SUM(SUM(x))` / `COUNT(COUNT(x))` unwrapped
///    to a single function call.
/// 4. **GROUP BY repair** — literal-null entries removed, non-aggregated
///    SELECT columns added.
pub fn schema_aware_fix(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool {
    let mut changed = false;

    // Step 0: fix embedded arithmetic before any other analysis
    // so the resulting BinaryOp sub-expressions are visible to the
    // join, aggregate, and GROUP BY fixers.
    changed |= columns::fix_arithmetic_column_refs(plan);
    // Step 1: fix column references that point to wrong tables via FK.
    // Must run BEFORE fix_missing_joins so the rewritten table triggers
    // the correct join to be added.
    changed |= column_table::fix_column_table(plan, schema);
    // Step 2: add missing joins for tables referenced but not in FROM/JOINS.
    changed |= joins::fix_missing_joins(plan, schema);
    // Step 3: remove nested aggregate duplication.
    changed |= aggregates::deduplicate_nested_aggregates(plan);
    // Step 4: repair GROUP BY clauses.
    changed |= group_by::fix_group_by(plan);

    if changed {
        debug!(target: "vlorql_core::fix", "schema_aware_fix applied changes");
    }
    changed
}
