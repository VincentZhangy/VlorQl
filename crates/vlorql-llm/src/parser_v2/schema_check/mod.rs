//! JSON Schema validation layer for the V2 parse pipeline.
//!
//! Validates normalized JSON against the canonical QueryPlan schema,
//! producing precise structural error messages for retry prompts.

use schemars::schema_for;
use serde_json::Value;

/// Validate a normalized JSON value against the QueryPlan JSON Schema.
///
/// Returns `Ok(())` when the value conforms, or `Err` with human-readable
/// structural errors.
pub fn validate_against_schema(val: &Value) -> Result<(), Vec<String>> {
    let schema = schema_for!(vlorql_core::schema::QueryPlan);
    let schema_value = match serde_json::to_value(&schema) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("Failed to serialize schema: {e}")]),
    };

    let compiled = match jsonschema::Validator::new(&schema_value) {
        Ok(c) => c,
        Err(e) => return Err(vec![format!("Failed to compile schema: {e}")]),
    };

    let errors: Vec<String> = compiled
        .iter_errors(val)
        .map(|e| format!("Schema violation: {e}"))
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
