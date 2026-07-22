//! Field-name alias canonicalization.
//!
//! Maps LLM output field names to canonical QueryPlan field names.
//! This is the core of the LLM Compatibility Layer: add an entry here
//! when a new model uses a different name for a standard concept.
//!
//! # Examples
//!
//! ```ignore
//! use serde_json::json;
//!
//! let mut val = json!({"filter": {"type": "comparison", …}});
//! aliases::normalize_field_names(&mut val);
//! assert!(val.get("where").is_some());
//! ```

/// A single alias entry: maps a non-standard field name to its
/// canonical form, optionally scoped to a specific model.
#[derive(Debug, Clone)]
pub struct AliasEntry {
    /// The non-standard field name used by some LLM outputs.
    pub from: &'static str,
    /// The canonical QueryPlan field name.
    pub to: &'static str,
    /// When `Some(model)`, this alias only applies to that model.
    /// When `None`, it applies to all models (global).
    pub model: Option<&'static str>,
}

impl AliasEntry {
    /// Create a global alias entry (applies to all models).
    pub const fn global(from: &'static str, to: &'static str) -> Self {
        Self {
            from,
            to,
            model: None,
        }
    }

    /// Create a model-specific alias entry.
    pub const fn for_model(from: &'static str, to: &'static str, model: &'static str) -> Self {
        Self {
            from,
            to,
            model: Some(model),
        }
    }
}

/// Global alias table — applies to all models.
///
/// Add entries here when a new model uses a different field name for a
/// standard QueryPlan concept.
pub const FIELD_ALIASES: &[AliasEntry] = &[
    // ── WHERE 相关 ────────────────────────────────────────────────
    AliasEntry::global("filter", "where"),
    AliasEntry::global("filters", "where"),
    AliasEntry::global("conditions", "where"),
    AliasEntry::global("condition", "where"),
    AliasEntry::global("predicate", "where"),
    AliasEntry::global("predicates", "where"),
    // ── SELECT 相关 ────────────────────────────────────────────────
    AliasEntry::global("projection", "select"),
    AliasEntry::global("projections", "select"),
    AliasEntry::global("columns", "select"),
    AliasEntry::global("project", "select"),
    AliasEntry::global("project_list", "select"),
    AliasEntry::global("fields", "select"),
    // ── FROM 相关 ──────────────────────────────────────────────────
    AliasEntry::global("source", "from"),
    AliasEntry::global("sources", "from"),
    AliasEntry::global("tables", "from"),
    // ── ORDER BY 相关 ──────────────────────────────────────────────
    AliasEntry::global("sort", "order_by"),
    AliasEntry::global("sorts", "order_by"),
    AliasEntry::global("sort_by", "order_by"),
    AliasEntry::global("ordering", "order_by"),
    AliasEntry::global("order", "order_by"),
    // ── GROUP BY 相关 ──────────────────────────────────────────────
    AliasEntry::global("group", "group_by"),
    AliasEntry::global("groupby", "group_by"),
    // ── HAVING 相关 ────────────────────────────────────────────────
    AliasEntry::global("having_condition", "having"),
    // ── LIMIT / OFFSET 相关 ────────────────────────────────────────
    AliasEntry::global("limit_count", "limit"),
    AliasEntry::global("max_rows", "limit"),
    AliasEntry::global("max_results", "limit"),
    AliasEntry::global("top", "limit"),
    AliasEntry::global("offset_count", "offset"),
    AliasEntry::global("skip", "offset"),
    AliasEntry::global("start", "offset"),
    // ── JOIN 相关 ──────────────────────────────────────────────────
    AliasEntry::global("join", "joins"),
    AliasEntry::global("relations", "joins"),
    // ── CTE 相关 ───────────────────────────────────────────────────
    AliasEntry::global("cte", "ctes"),
    AliasEntry::global("with", "ctes"),
    // ── 通用字段重命名 ─────────────────────────────────────────────
    AliasEntry::global("col", "column"),
    AliasEntry::global("kind", "type"),
    AliasEntry::global("field", "column"),
    AliasEntry::global("table_name", "table"),
    AliasEntry::global("alias_name", "alias"),
    AliasEntry::global("desc", "descending"),
    AliasEntry::global("operator", "op"),
    AliasEntry::global("comparisons", "comparison"),
];

/// Model-specific aliases.  These are checked **after** the global
/// table and take precedence when the model matches.
pub const MODEL_ALIASES: &[AliasEntry] = &[
    // DeepSeek sometimes uses "condition" for the where clause
    // (already covered by global alias "condition" → "where").
    //
    // Qwen sometimes emits "table_name" for the from clause
    // (already covered by global alias "table_name" → "table").
];

/// Recursively rename fields in a JSON value according to the alias
/// tables.
///
/// Returns `true` if any field was renamed.
///
/// When `model` is `Some(...)`, model-specific aliases are also
/// applied.  Global aliases are always applied.
#[must_use]
pub fn normalize_field_names(val: &mut serde_json::Value) -> bool {
    normalize_field_names_impl(val, None)
}

/// Recursively rename fields, including model-specific aliases.
///
/// Returns `true` if any field was renamed.
#[must_use]
pub fn normalize_field_names_for_model(val: &mut serde_json::Value, model: &str) -> bool {
    normalize_field_names_impl(val, Some(model))
}

fn normalize_field_names_impl(val: &mut serde_json::Value, model: Option<&str>) -> bool {
    let mut changed = false;
    match val {
        serde_json::Value::Object(map) => {
            // Collect renames first (avoid double-borrow).
            let mut renames: Vec<(String, String)> = Vec::new();
            for key in map.keys() {
                // Check model-specific aliases first (higher priority).
                let mut matched = false;
                if let Some(model) = model {
                    for entry in MODEL_ALIASES {
                        if entry.model == Some(model) && key == entry.from {
                            if !map.contains_key(entry.to) {
                                renames.push((key.clone(), entry.to.to_owned()));
                                matched = true;
                            }
                            break;
                        }
                    }
                }
                if !matched {
                    // Check global aliases.
                    for entry in FIELD_ALIASES {
                        if entry.model.is_none() && key == entry.from {
                            if !map.contains_key(entry.to) {
                                renames.push((key.clone(), entry.to.to_owned()));
                            }
                            break;
                        }
                    }
                }
            }
            for (old_key, new_key) in &renames {
                if let Some(v) = map.remove(old_key) {
                    map.insert(new_key.clone(), v);
                    changed = true;
                }
            }
            // Recurse into children.
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in &keys {
                if let Some(v) = map.get_mut(k) {
                    changed |= normalize_field_names_impl(v, model);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                changed |= normalize_field_names_impl(v, model);
            }
        }
        _ => {}
    }
    changed
}

/// Look up the canonical name for a given alias.
///
/// Returns `Some(canonical)` if the name is an alias, `None` if it is
/// already canonical or unknown.
#[must_use]
pub fn resolve_alias(name: &str) -> Option<&'static str> {
    FIELD_ALIASES
        .iter()
        .find(|entry| entry.model.is_none() && entry.from == name)
        .map(|entry| entry.to)
}

/// Returns the complete global alias table as a slice of `(from, to)` pairs.
#[must_use]
pub fn global_aliases() -> Vec<(&'static str, &'static str)> {
    FIELD_ALIASES
        .iter()
        .filter(|e| e.model.is_none())
        .map(|e| (e.from, e.to))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_filter_to_where() {
        let mut val = json!({"filter": {"type": "comparison", "left": {"column": "age"}, "op": "gt", "right": {"value": 18}}});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("where").is_some(), "filter should become where");
        assert!(val.get("filter").is_none(), "filter should be removed");
    }

    #[test]
    fn normalizes_projection_to_select() {
        let mut val = json!({"projection": [{"type": "star"}], "from": {"table": "users"}});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("select").is_some());
        assert!(val.get("projection").is_none());
    }

    #[test]
    fn normalizes_sort_to_order_by() {
        let mut val = json!({"sort": [{"expr": {"column": "name"}, "descending": true}], "select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("order_by").is_some());
        assert!(val.get("sort").is_none());
    }

    #[test]
    fn normalizes_kind_to_type() {
        let mut val = json!({"kind": "column_ref", "table": "users", "column": "id"});
        assert!(normalize_field_names(&mut val));
        assert_eq!(val.get("type").and_then(|v| v.as_str()), Some("column_ref"));
        assert!(val.get("kind").is_none());
    }

    #[test]
    fn normalizes_desc_to_descending() {
        let mut val = json!({"expr": {"column": "name"}, "desc": true});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("descending").is_some());
        assert!(val.get("desc").is_none());
    }

    #[test]
    fn does_not_overwrite_existing_canonical_field() {
        let mut val = json!({"where": {"type": "comparison"}, "filter": {"type": "and"}});
        // "filter" → "where" but "where" already exists — alias is skipped.
        // The old "filter" field stays (it will be stripped by structure
        // normalization, not by alias normalization).
        assert!(!normalize_field_names(&mut val));
        // "filter" should still be present (alias was skipped, not removed).
        assert!(
            val.get("filter").is_some(),
            "filter stays when where already exists"
        );
        // The original "where" should still be there.
        assert_eq!(
            val.get("where")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn already_canonical_no_change() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}});
        assert!(!normalize_field_names(&mut val));
    }

    #[test]
    fn normalizes_recursively() {
        let mut val = json!({
            "select": [{"type": "star"}],
            "from": {"table": "users"},
            "filter": {
                "type": "and",
                "left": {"kind": "comparison", "field": "age", "operator": "gt", "right": {"value": 18}},
                "right": {"kind": "comparison", "field": "status", "operator": "eq", "right": {"value": "active"}}
            }
        });
        assert!(normalize_field_names(&mut val));
        // "filter" → "where"
        let where_obj = val.get("where").unwrap();
        assert_eq!(where_obj.get("type").and_then(|v| v.as_str()), Some("and"));
        // "kind" → "type" (inside where's children)
        let left = where_obj.get("left").unwrap();
        assert_eq!(
            left.get("type").and_then(|v| v.as_str()),
            Some("comparison")
        );
        assert!(left.get("kind").is_none());
        // "field" → "column"
        assert_eq!(left.get("column").and_then(|v| v.as_str()), Some("age"));
        assert!(left.get("field").is_none());
        // "operator" → "op"
        assert_eq!(left.get("op").and_then(|v| v.as_str()), Some("gt"));
        assert!(left.get("operator").is_none());
    }

    #[test]
    fn resolve_alias_works() {
        assert_eq!(resolve_alias("filter"), Some("where"));
        assert_eq!(resolve_alias("projection"), Some("select"));
        assert_eq!(resolve_alias("sort"), Some("order_by"));
        assert_eq!(resolve_alias("where"), None); // already canonical
        assert_eq!(resolve_alias("select"), None); // already canonical
        assert_eq!(resolve_alias("nonexistent"), None);
    }

    #[test]
    fn global_aliases_returns_all() {
        let aliases = global_aliases();
        assert!(aliases.contains(&("filter", "where")));
        assert!(aliases.contains(&("projection", "select")));
        assert!(aliases.contains(&("sort", "order_by")));
    }

    #[test]
    fn model_specific_normalize() {
        // Add a model-specific alias for testing.
        // We use the existing MODEL_ALIASES (which is empty in production).
        // For this test, we verify the function works with model=None.
        let mut val = json!({"filter": {"type": "comparison"}});
        assert!(normalize_field_names_for_model(&mut val, "deepseek"));
        assert!(val.get("where").is_some());
    }

    #[test]
    fn normalizes_source_to_from() {
        let mut val = json!({"select": [{"type": "star"}], "source": {"table": "users"}});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("from").is_some());
        assert!(val.get("source").is_none());
    }

    #[test]
    fn normalizes_order_to_order_by() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "order": [{"expr": {"column": "name"}, "descending": true}]});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("order_by").is_some());
        assert!(val.get("order").is_none());
    }

    #[test]
    fn normalizes_group_to_group_by() {
        let mut val = json!({"select": [{"type": "star"}], "from": {"table": "users"}, "group": [{"column": "status"}]});
        assert!(normalize_field_names(&mut val));
        assert!(val.get("group_by").is_some());
        assert!(val.get("group").is_none());
    }

    #[test]
    fn empty_object_no_change() {
        let mut val = json!({});
        assert!(!normalize_field_names(&mut val));
    }

    #[test]
    fn null_value_no_change() {
        let mut val = serde_json::Value::Null;
        assert!(!normalize_field_names(&mut val));
    }
}
