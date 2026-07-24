//! Projection / SELECT builder: canonical JSON → [`Projection`].

use serde_json::Value;
use vlorql_core::schema::Projection;

use super::expr_builder::{BuildError, build_expression, opt_str, req_obj, req_str, type_name};

/// Build a [`Projection`] from a canonical JSON object.
///
/// Expected shapes:
/// - `{"type": "column_ref", "table": "users", "column": "id", "alias": "u"}`
/// - `{"type": "expr", "expression": {...}, "alias": "total"}`
/// - `{"type": "star", "table": "users"}`
pub fn build_projection(val: &Value) -> Result<Projection, BuildError> {
    let obj = val.as_object().ok_or_else(|| {
        BuildError::new(
            "",
            format!("expected object for Projection, got {}", type_name(val)),
        )
    })?;
    let type_str = req_str(obj, "type", "")?;
    match type_str {
        "column_ref" => {
            let column = req_str(obj, "column", "")?.to_owned();
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            let alias = opt_str(obj, "alias").map(|s| s.to_owned());
            Ok(Projection::Column {
                table,
                column,
                alias,
            })
        }
        "expr" => {
            let e = req_obj(
                obj.get("expression").ok_or_else(|| {
                    BuildError::new("", "missing `expression` field on expr projection")
                })?,
                "expression",
            )?;
            let expression = build_expression(&Value::Object(e.clone()))?;
            let alias = opt_str(obj, "alias").map(|s| s.to_owned());
            Ok(Projection::Expr { expression, alias })
        }
        "star" => {
            let table = opt_str(obj, "table").map(|s| s.to_owned());
            Ok(Projection::Star { table })
        }
        // Fallback: treat bare expression types (function_call, binary_op, etc.)
        // as an Expr projection.  The normalize layer should have wrapped these
        // already, but this handles edge cases where normalization missed one.
        "function_call" | "FunctionCall"
        | "binary_op" | "BinaryOp"
        | "case" | "Case"
        | "literal" | "Literal"
        | "subquery" | "SubQuery" => {
            let expression = build_expression(val)
                .map_err(|e| e.at("expression"))?;
            let alias = opt_str(obj, "alias").map(|s| s.to_owned());
            Ok(Projection::Expr { expression, alias })
        }
        other => Err(BuildError::new(
            "type",
            format!("unknown Projection variant `{other}`"),
        )),
    }
}

/// Build a vector of [`Projection`] from a canonical JSON array.
pub fn build_projections(arr: &[Value]) -> Result<Vec<Projection>, BuildError> {
    arr.iter()
        .enumerate()
        .map(|(i, v)| build_projection(v).map_err(|e| e.at(&format!("select[{i}]"))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_column_ref_projection() {
        let val = json!({"type": "column_ref", "table": "users", "column": "id"});
        let proj = build_projection(&val).unwrap();
        assert!(
            matches!(proj, Projection::Column { table: Some(t), column: c, .. } if t == "users" && c == "id")
        );
    }

    #[test]
    fn build_column_ref_with_alias() {
        let val = json!({"type": "column_ref", "column": "name", "alias": "user_name"});
        let proj = build_projection(&val).unwrap();
        assert!(matches!(proj, Projection::Column { alias: Some(a), .. } if a == "user_name"));
    }

    #[test]
    fn build_expr_projection() {
        let val = json!({"type": "expr", "expression": {"type": "literal", "value": 42, "data_type": "int"}, "alias": "answer"});
        let proj = build_projection(&val).unwrap();
        assert!(matches!(proj, Projection::Expr { .. }));
    }

    #[test]
    fn build_star_projection() {
        let val = json!({"type": "star"});
        let proj = build_projection(&val).unwrap();
        assert!(matches!(proj, Projection::Star { table: None }));
    }

    #[test]
    fn build_star_projection_with_table() {
        let val = json!({"type": "star", "table": "users"});
        let proj = build_projection(&val).unwrap();
        assert!(matches!(proj, Projection::Star { table: Some(t) } if t == "users"));
    }

    #[test]
    fn build_projections_array() {
        let arr = json!([
            {"type": "column_ref", "column": "id"},
            {"type": "column_ref", "column": "name"},
            {"type": "star"}
        ]);
        let projections = build_projections(arr.as_array().unwrap()).unwrap();
        assert_eq!(projections.len(), 3);
    }

    #[test]
    fn error_on_unknown_type() {
        let val = json!({"type": "invalid"});
        let result = build_projection(&val);
        assert!(result.is_err());
    }

    #[test]
    fn error_on_missing_column() {
        let val = json!({"type": "column_ref"});
        let result = build_projection(&val);
        assert!(result.is_err());
    }
}
