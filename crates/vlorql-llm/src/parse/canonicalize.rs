//! Structural canonicalize of QueryPlan-shaped JSON.
//!
//! Transforms messy LLM JSON into a *canonical* `serde_json::Value` that is
//! as close as possible to the wire shape of [`vlorql_core::schema::QueryPlan`].
//!
//! This layer must **not** invent business semantics (e.g. schema-aware
//! auto-joins should eventually live in validation). For now the existing
//! `repair_*` logic lives here unchanged so behaviour is preserved while the
//! pipeline is split.

/// Attempts to repair structural issues in a QueryPlan JSON produced
/// by small LLMs.
///
/// Common issues repaired:
///
/// - **Misplaced fields**: `order_by`, `limit`, `offset`, `group_by`,
///   `having` inside the `where` object are moved to the top level.
/// - **Array predicates**: `left` / `right` inside `and` / `or` that
///   are arrays are unwrapped to a single object (first element wins).
/// - **Null / empty entries**: removed from predicates.
///
/// Returns the input unchanged when no repair is needed.
#[must_use]
pub fn repair_query_plan_json(content: &str) -> std::borrow::Cow<'_, str> {
    let mut value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return std::borrow::Cow::Borrowed(content),
    };

    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return std::borrow::Cow::Borrowed(content),
    };

    let changed = repair_query_plan_object(obj);

    if changed {
        std::borrow::Cow::Owned(
            serde_json::to_string(&value).unwrap_or_else(|_| content.to_owned()),
        )
    } else {
        std::borrow::Cow::Borrowed(content)
    }
}

/// Field-name synonym table: LLM output field → canonical field.
///
/// Add entries here when a new model uses a different field name for a
/// standard QueryPlan concept.  This table is consulted **before** any
/// structural repair runs, so repairs see already-normalized names.
const FIELD_ALIASES: &[(&str, &str)] = &[
    ("filter", "where"),
    ("filters", "where"),
    ("conditions", "where"),
    ("predicate", "where"),
    ("predicates", "where"),
    ("projection", "select"),
    ("projections", "select"),
    ("columns", "select"),
    ("col", "column"),
    ("sort", "order_by"),
    ("sorts", "order_by"),
    ("sort_by", "order_by"),
    ("ordering", "order_by"),
    ("kind", "type"),
    ("field", "column"),
    ("table_name", "table"),
    ("alias_name", "alias"),
    ("limit_count", "limit"),
    ("offset_count", "offset"),
    ("desc", "descending"),
    ("operator", "op"),
    ("condition", "comparison"),
    ("comparisons", "comparison"),
];

/// Recursively rename fields in a JSON value according to `FIELD_ALIASES`.
fn normalize_field_names(val: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match val {
        serde_json::Value::Object(map) => {
            // Collect renames first (avoid double-borrow).
            let mut renames: Vec<(String, String)> = Vec::new();
            for key in map.keys() {
                for &(from, to) in FIELD_ALIASES {
                    if key == from {
                        renames.push((key.clone(), to.to_owned()));
                        break;
                    }
                }
            }
            for (old_key, new_key) in &renames {
                if !map.contains_key(new_key) {
                    if let Some(v) = map.remove(old_key) {
                        map.insert(new_key.clone(), v);
                        changed = true;
                    }
                }
            }
            // Recurse into children (after potential rename so children see new names).
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in &keys {
                if let Some(v) = map.get_mut(k) {
                    changed |= normalize_field_names(v);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                changed |= normalize_field_names(v);
            }
        }
        _ => {}
    }
    changed
}
fn repair_query_plan_object(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut changed = false;

    // --- 0. Normalize field names via synonym table ---
    //     Must run before any structural step so repairs see canonical names.
    {
        let mut wrapper = serde_json::Value::Object(std::mem::take(obj));
        changed |= normalize_field_names(&mut wrapper);
        if let serde_json::Value::Object(map) = wrapper {
            let _ = std::mem::replace(obj, map);
        }
    }

    // --- 1. Move misplaced top-level fields from inside `where` ---
    const TOP_LEVEL_FIELDS: &[&str] = &[
        "order_by", "limit", "offset", "group_by", "having", "joins", "ctes",
    ];

    // Collect misplaced fields from `where` first (before any mutable borrow of `obj`).
    let mut extracted: Vec<(String, serde_json::Value)> = Vec::new();

    if let Some(where_val) = obj.get_mut("where")
        && let Some(where_obj) = where_val.as_object_mut()
    {
        for &field in TOP_LEVEL_FIELDS {
            if let Some(val) = where_obj.remove(field) {
                if !val.is_null() && !is_empty_array(&val) {
                    extracted.push((field.to_owned(), val));
                }
                changed = true;
            }
        }

        // --- 2. Recursively fix array predicates inside `where` ---
        changed |= repair_predicate_object(where_val);
    }

    // Now insert the extracted fields at the top level (separate borrow).
    for (field, val) in &extracted {
        if !obj.contains_key(field) {
            obj.insert(field.clone(), val.clone());
        }
    }

    // --- 1b. Strip unknown top-level fields that `QueryPlan` rejects ---
    //     `QueryPlan` uses `#[serde(deny_unknown_fields)]`, so fields like
    //     `right`, `left`, `op`, `child`, `expr` at the plan level cause an
    //     immediate deserialization error.  Remove them so the repair below
    //     can at least attempt to make `where` valid.
    const PLAN_FIELDS: &[&str] = &[
        "select", "from", "where", "group_by", "having", "order_by", "limit", "offset", "joins",
        "ctes",
    ];
    let before = obj.len();
    obj.retain(|key, _| PLAN_FIELDS.contains(&key.as_str()));
    if obj.len() != before {
        changed = true;
    }

    // --- 3. Recursively repair predicates inside `having` ---
    //     Also handles `"having": [null]` or `"having": [Predicate]` emitted by
    //     the LLM when it mistakenly wraps the predicate in an array.
    if let Some(having) = obj.get_mut("having") {
        if having.is_array() {
            let arr = having.as_array().unwrap();
            let pred = arr
                .iter()
                .filter_map(|v| v.as_object())
                .find(|o| o.contains_key("type"))
                .cloned()
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null);
            if pred.is_null() {
                obj.remove("having");
            } else {
                obj.insert("having".to_owned(), pred);
            }
            changed = true;
        } else {
            changed |= repair_predicate_object(having);
        }
    }

    // --- 3.5. Extract misplaced plan-level fields from inside JOINs ---
    //     The LLM sometimes nests `where`, `order_by`, `limit` etc. inside a
    //     join object instead of at the top level.  Extract them back to the
    //     plan level before step 4 strips them as unknown join fields.
    const PLAN_LEVEL_FIELDS: &[&str] = &[
        "select", "from", "where", "group_by", "having", "order_by", "limit", "offset", "ctes",
    ];
    // Collect first (read-only borrow of joins), then apply (mutable borrow of obj).
    let mut extracted_from_joins: Vec<(String, serde_json::Value)> = Vec::new();
    if let Some(joins) = obj.get("joins").and_then(|v| v.as_array()) {
        for join in joins {
            if let Some(join_obj) = join.as_object() {
                for &field in PLAN_LEVEL_FIELDS {
                    if let Some(val) = join_obj.get(field) {
                        if !val.is_null() && !is_empty_array(val) {
                            extracted_from_joins.push((field.to_owned(), val.clone()));
                        }
                    }
                }
            }
        }
    }
    for (field, val) in &extracted_from_joins {
        if !obj.contains_key(field) {
            obj.insert(field.clone(), val.clone());
            changed = true;
        }
    }
    // Also remove the extracted fields from the join objects themselves.
    if let Some(joins) = obj.get_mut("joins").and_then(|v| v.as_array_mut()) {
        for join in joins.iter_mut() {
            if let Some(join_obj) = join.as_object_mut() {
                for &field in PLAN_LEVEL_FIELDS {
                    if join_obj.remove(field).is_some() {
                        changed = true;
                    }
                }
            }
        }
    }

    // --- 4. Recursively repair predicates inside joins ---
    //     Also strips `left_table` (not a field on JoinClause) and any other
    //     unknown join-level fields the LLM may hallucinate, and removes
    //     non-object entries (strings, numbers, nulls) that the LLM injects.
    const VALID_JOIN_FIELDS: &[&str] = &["join_type", "right_table", "on"];
    if let Some(joins) = obj.get_mut("joins")
        && let Some(joins_arr) = joins.as_array_mut()
    {
        let before = joins_arr.len();
        joins_arr.retain(|v| v.is_object());
        if joins_arr.len() != before {
            changed = true;
        }
        if joins_arr.is_empty() {
            obj.remove("joins");
            changed = true;
        } else {
            for join in joins_arr.iter_mut() {
                if let Some(join_obj) = join.as_object_mut() {
                    join_obj.retain(|key, _| VALID_JOIN_FIELDS.contains(&key.as_str()));

                    // Convert bare-string `right_table` to FromClause object.
                    if let Some(rt) = join_obj.get("right_table").and_then(|v| v.as_str()) {
                        join_obj.insert("right_table".to_owned(), serde_json::json!({"table": rt}));
                        changed = true;
                    }

                    // Infer missing `right_table` from the ON clause when possible.
                    if !join_obj.contains_key("right_table") && join_obj.contains_key("on") {
                        if let Some(on_obj) = join_obj.get("on").and_then(|v| v.as_object()) {
                            let on_table = on_obj
                                .get("right")
                                .and_then(|r| r.as_object())
                                .and_then(|r| r.get("table"))
                                .and_then(|t| t.as_str());
                            if let Some(table) = on_table {
                                join_obj.insert(
                                    "right_table".to_owned(),
                                    serde_json::json!({"table": table}),
                                );
                                changed = true;
                            }
                        }
                    }

                    let right_table_name = join_obj
                        .get("right_table")
                        .and_then(|rt| rt.as_object())
                        .and_then(|rt| rt.get("table"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    if let Some(on) = join_obj.get_mut("on") {
                        // Handle bare ColumnRef in join ON clause.
                        // The LLM sometimes emits duplicate-keyed objects like
                        // {"table":"users","column":"id","table":"orders","column":"user_id"}
                        // which JSON parsing resolves to just {"table":"orders","column":"user_id"}.
                        // Reconstruct a proper comparison using right_table info.
                        if let Some(on_obj) = on.as_object() {
                            if !on_obj.contains_key("type") && on_obj.contains_key("column") {
                                let mut left_expr = on.clone();
                                repair_expression_value(&mut left_expr);
                                *on = serde_json::json!({
                                    "type": "comparison",
                                    "left": left_expr,
                                    "op": "eq",
                                    "right": {
                                        "type": "column_ref",
                                        "table": right_table_name,
                                        "column": "id"
                                    }
                                });
                                changed = true;
                                continue;
                            }
                        }
                        changed |= repair_predicate_object(on);
                    }
                }
            }
        }
    }

    // --- 5. Wrap top-level `descending` + `expr` into `order_by` ---
    // The LLM sometimes emits `descending` and `expr` at the top level of the
    // QueryPlan instead of inside an `OrderByTerm` within the `order_by` array.
    if !obj.contains_key("order_by") {
        if let (Some(expr), Some(descending)) = (obj.remove("expr"), obj.remove("descending"))
            && descending.is_boolean()
        {
            let term = serde_json::json!({
                "expr": expr,
                "descending": descending,
            });
            obj.insert("order_by".to_owned(), serde_json::json!([term]));
            changed = true;
        }
    }

    // --- 6. Remove null / invalid elements from array fields ---
    for array_field in &["group_by", "order_by"] {
        if let Some(arr) = obj.get_mut(*array_field).and_then(|v| v.as_array_mut()) {
            let len_before = arr.len();
            arr.retain(|v| !v.is_null());
            if arr.len() != len_before {
                changed = true;
            }
            if arr.is_empty() {
                obj.remove(*array_field);
                changed = true;
            }
        }
    }

    // --- 7. Collapse `where` from array to single predicate ---
    //     llama3.2 sometimes emits `"where": [{...}, "garbage string"]`
    let where_array_pred: Option<serde_json::Value> =
        obj.get("where").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_object())
                .find(|o| o.contains_key("type"))
                .cloned()
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null)
        });

    if let Some(pred) = where_array_pred {
        if pred.is_null() {
            obj.remove("where");
        } else {
            obj.insert("where".to_owned(), pred);
        }
        changed = true;
    }

    // --- 7b. Recursively repair the collapsed `where` predicate ---
    //     The collapsed object may still have array-valued `left`/`right`/`child`
    //     fields (e.g. `"left": [{...}]`), which `repair_predicate_object` fixes.
    if let Some(where_val) = obj.get_mut("where") {
        changed |= repair_predicate_object(where_val);
    }

    // --- 7c. Inject default `select` when missing ---
    //     qwen2.5 等小模型有时会省略子查询的 select 字段。
    if !obj.contains_key("select") && obj.contains_key("from") {
        obj.insert("select".to_owned(), serde_json::json!([{"type": "star"}]));
        changed = true;
    }

    // --- 8. Repair and remove invalid elements from `select` ---
    //     First inject missing `type` tags for items that look like
    //     ColumnRef, then remove any remaining invalid items.
    const VALID_PROJECTION_TYPES: &[&str] = &["column_ref", "expr", "star"];
    if let Some(arr) = obj.get_mut("select").and_then(|v| v.as_array_mut()) {
        let len_before = arr.len();
        // Inject missing `type` for items that look like ColumnRef
        for item in arr.iter_mut() {
            if let Some(item_obj) = item.as_object_mut() {
                if !item_obj.contains_key("type") && item_obj.contains_key("column") {
                    item_obj.insert(
                        "type".to_owned(),
                        serde_json::Value::String("column_ref".to_owned()),
                    );
                }
            }
        }
        // Remove items that still have invalid or missing type
        arr.retain(|v| {
            v.as_object()
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .is_some_and(|t| VALID_PROJECTION_TYPES.contains(&t))
        });
        if arr.len() != len_before {
            changed = true;
        }
    }

    // --- 9. Fix missing `type` tags on expression fields in group_by / order_by ---
    //     The LLM often omits `type` from Expression objects in these positions.
    for array_field in &["group_by", "order_by"] {
        if let Some(arr) = obj.get_mut(*array_field).and_then(|v| v.as_array_mut()) {
            // First, flatten `{"type":"array","items":[...]}` objects that the
            // LLM sometimes emits instead of a bare `[Expression]` array.
            let mut flat: Vec<serde_json::Value> = Vec::new();
            for item in arr.drain(..) {
                if let Some(map) = item.as_object()
                    && map.get("type").and_then(|t| t.as_str()) == Some("array")
                    && let Some(items) = map.get("items").and_then(|v| v.as_array())
                {
                    flat.extend(items.iter().cloned());
                    changed = true;
                } else {
                    flat.push(item);
                }
            }
            *arr = flat;

            for item in arr.iter_mut() {
                if let Some(term) = item.as_object_mut()
                    && term.contains_key("expr")
                    && !term.contains_key("type")
                {
                    // group_by items are bare Expression objects, BUT the LLM
                    // often emits them in order_by format: {"expr": {...}}.
                    // Unwrap the expr value to become the item itself.
                    if let Some(expr) = term.remove("expr") {
                        *item = expr;
                        changed = true;
                        // Now repair the unwrapped expression.
                        changed |= repair_expression_value(item);
                    }
                } else if let Some(term) = item.as_object_mut()
                    && term.contains_key("expr")
                {
                    // order_by items are OrderByTerm with an `expr` Expression
                    if let Some(expr) = term.get_mut("expr") {
                        changed |= repair_expression_value(expr);
                    }
                } else {
                    // group_by items are bare Expression objects
                    changed |= repair_expression_value(item);
                }
            }
        }
    }

    // --- 9.5. Expand `SELECT *` to GROUP BY columns when GROUP BY is present ---
    //     Small LLMs often emit `{"select":[{"type":"star"}], "group_by":[...]}`
    //     which is invalid SQL (SELECT * with GROUP BY).  Replace each `*` with
    //     the GROUP BY column references so the plan compiles to valid SQL.
    let gb_columns: Vec<serde_json::Value> = obj
        .get("group_by")
        .and_then(|v| v.as_array())
        .filter(|v| !v.is_empty())
        .map(|arr| arr.iter().filter_map(expand_group_by_expr).collect())
        .unwrap_or_default();
    if !gb_columns.is_empty() {
        if let Some(select) = obj.get_mut("select").and_then(|v| v.as_array_mut()) {
            let has_star = select.iter().any(|v| {
                v.as_object()
                    .and_then(|o| o.get("type"))
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == "star")
            });
            if has_star {
                let mut new_select: Vec<serde_json::Value> = Vec::new();
                for item in select.drain(..) {
                    let is_star = item
                        .as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t == "star");
                    if is_star {
                        new_select.extend(gb_columns.clone());
                    } else {
                        new_select.push(item);
                    }
                }
                *select = new_select;
                changed = true;
            }
        }
    }

    // --- 10. (removed) Auto-join inference was too speculative; pushing to
    //          validator / retry layer in future iterations.

    // --- 11. Normalize data_type strings (e.g. "integer" → "int") ---
    let mut plan_value = serde_json::Value::Object(std::mem::take(obj));
    changed |= normalize_data_types(&mut plan_value);
    if let serde_json::Value::Object(map) = plan_value {
        *obj = map;
    }

    changed
}

/// Converts a GROUP BY `Expression` into a SELECT `Projection` (column_ref).
///
/// Only `column_ref` expressions can be safely expanded — other expression types
/// (literals, function calls) are skipped so the plan keeps the star and fails
/// validation (which produces a clearer error).
fn expand_group_by_expr(expr: &serde_json::Value) -> Option<serde_json::Value> {
    let obj = expr.as_object()?;
    if obj.get("type")?.as_str()? != "column_ref" {
        return None;
    }
    let mut proj = serde_json::Map::new();
    proj.insert(
        "type".to_owned(),
        serde_json::Value::String("column_ref".to_owned()),
    );
    proj.insert(
        "table".to_owned(),
        obj.get("table").cloned().unwrap_or(serde_json::Value::Null),
    );
    proj.insert(
        "column".to_owned(),
        obj.get("column")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    proj.insert("alias".to_owned(), serde_json::Value::Null);
    Some(serde_json::Value::Object(proj))
}

/// Normalizes common LLM data_type aliases to the canonical serde form.
fn normalize_data_types(val: &mut serde_json::Value) -> bool {
    let mut changed = false;
    normalize_data_types_inner(val, &mut changed);
    changed
}

fn normalize_data_types_inner(val: &mut serde_json::Value, changed: &mut bool) {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(dt) = map.get("data_type").and_then(|v| v.as_str()) {
                let normalized = match dt {
                    "integer" | "int4" | "int8" | "bigint" | "smallint" | "tinyint" => "int",
                    "varchar" | "text" | "char" | "character" | "character varying" => "string",
                    "decimal" | "numeric" | "real" | "double" | "double precision" => "float",
                    "bool" | "boolean" => "boolean",
                    "timestampz"
                    | "timestamptz"
                    | "datetime"
                    | "timestamp with time zone"
                    | "timestamp without time zone"
                    | "date" => "timestamp",
                    "NULL" | "Null" => "null",
                    _ => dt,
                };
                if normalized != dt {
                    map.insert(
                        "data_type".to_owned(),
                        serde_json::Value::String(normalized.to_owned()),
                    );
                    *changed = true;
                }
            }
            // Recurse into all child values (but don't recurse from null/empty Vec entries).
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in &keys {
                if let Some(v) = map.get_mut(key) {
                    normalize_data_types_inner(v, changed);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_data_types_inner(v, changed);
            }
        }
        _ => {}
    }
}

/// Adds missing `"type"` tags to Expression-like JSON objects.
///
/// The LLM frequently omits the `type` discriminator from `ColumnRef`,
/// `Literal`, and `FunctionCall` objects. This function infers the
/// correct tag from the present fields so that serde can deserialize
/// the value as an [`Expression`](vlorql_core::schema::Expression).
fn repair_expression_value(val: &mut serde_json::Value) -> bool {
    let obj = match val.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    if obj.contains_key("type") {
        return false;
    }

    if obj.contains_key("column") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("column_ref".to_owned()),
        );
        return true;
    }

    if obj.contains_key("value") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("literal".to_owned()),
        );
        return true;
    }

    if obj.contains_key("name") && obj.contains_key("args") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("function_call".to_owned()),
        );
        return true;
    }

    false
}

/// Repairs a single `Predicate` value (may be `and`/`or` with array sides).
/// Recurses into nested predicates.
fn repair_predicate_object(pred: &mut serde_json::Value) -> bool {
    let obj = match pred.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let mut changed = false;
    let mut pred_type_str = obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_owned();

    // If the object has no `type` tag but has `left` and `op` fields,
    // it is a comparison predicate missing the type discriminator.
    // Immediately fix expression type tags inside it too.
    if pred_type_str.is_empty() && obj.contains_key("left") && obj.contains_key("op") {
        obj.insert(
            "type".to_owned(),
            serde_json::Value::String("comparison".to_owned()),
        );
        changed = true;
        for field in ["left", "right"] {
            if let Some(val) = obj.get_mut(field) {
                changed |= repair_expression_value(val);
            }
        }
        pred_type_str = "comparison".to_owned();
    }

    // (removed) Bare expression → `expr = NULL` inference was too
    // speculative; pushed to validator / retry layer.

    // Fix array-valued sides in and/or
    if pred_type_str == "and" || pred_type_str == "or" {
        for side in &["left", "right"] {
            if let Some(arr) = obj.get(*side).and_then(|v| v.as_array()) {
                if arr.is_empty() {
                    obj.remove(*side);
                    changed = true;
                } else {
                    let mut first = arr[0].clone();
                    repair_predicate_object(&mut first);
                    obj.insert((*side).to_string(), first);
                    changed = true;
                }
            } else if let Some(side_val) = obj.get_mut(*side) {
                changed |= repair_predicate_object(side_val);
            }
        }
    }

    // Fix array-valued `child` in `not`
    if pred_type_str == "not" {
        if let Some(arr) = obj.get("child").and_then(|v| v.as_array()) {
            if !arr.is_empty() {
                let mut first = arr[0].clone();
                repair_predicate_object(&mut first);
                obj.insert("child".to_owned(), first);
                changed = true;
            }
        } else if let Some(child) = obj.get_mut("child") {
            changed |= repair_predicate_object(child);
        }
    }

    // Recursively repair subquery plans inside `exists` / `in` predicates.
    // Small models (qwen2.5-coder, llama3.2) often emit subqueries without
    // a `select` field or with missing expression type tags.
    if pred_type_str == "exists" {
        if let Some(query) = obj.get_mut("query")
            && let Some(query_obj) = query.as_object_mut()
        {
            changed |= repair_query_plan_object(query_obj);
        }
    }
    if pred_type_str == "in" {
        if let Some(target) = obj.get_mut("target") {
            if let Some(target_obj) = target.as_object_mut() {
                // InTarget::SubQuery — repair the inner plan
                changed |= repair_query_plan_object(target_obj);
            } else if let Some(arr) = target.as_array_mut() {
                // InTarget::Values — fix expression type tags in each value
                for val in arr.iter_mut() {
                    changed |= repair_expression_value(val);
                }
            }
        }
    }

    // Fix array-valued expression fields
    if pred_type_str == "comparison"
        || pred_type_str == "between"
        || pred_type_str == "in"
        || pred_type_str == "like"
        || pred_type_str == "is_null"
    {
        for field in &["left", "right", "expr", "low", "high"] {
            if let Some(arr) = obj.get(*field).and_then(|v| v.as_array())
                && !arr.is_empty()
            {
                obj.insert((*field).to_string(), arr[0].clone());
                changed = true;
            }
        }
    }

    // Fix missing `type` tags on expression fields nested within predicates.
    // The LLM often emits bare `{"column":"x","table":"t"}` objects without
    // the `"type":"column_ref"` discriminator that serde needs.
    if pred_type_str == "comparison"
        || pred_type_str == "between"
        || pred_type_str == "in"
        || pred_type_str == "like"
        || pred_type_str == "is_null"
    {
        for field in &["left", "right", "expr", "low", "high"] {
            if let Some(val) = obj.get_mut(*field) {
                changed |= repair_expression_value(val);
            }
        }
    }

    // Safety net: inject missing `right` field on comparison predicates.
    // The LLM sometimes emits `{"left": ..., "op": "in"}` (or other ops)
    // without `right`.  Serde rejects the missing field with a parse error
    // that does not trigger a retry, so we inject a null literal to let
    // it deserialize; the validator will catch the semantic problem.
    if pred_type_str == "comparison" && !obj.contains_key("right") {
        obj.insert(
            "right".to_owned(),
            serde_json::json!({"type": "literal", "value": null, "data_type": "null"}),
        );
        changed = true;
    }

    // Simplify single-child `and`/`or`: if only `left` exists and no `right`,
    // replace the entire predicate with `left`.
    if (pred_type_str == "and" || pred_type_str == "or")
        && obj.contains_key("left")
        && !obj.contains_key("right")
        && let Some(left_val) = obj.remove("left")
    {
        *pred = left_val;
        changed = true;
    }

    changed
}

/// Returns `true` when `v` is an empty JSON array `[]`.
fn is_empty_array(v: &serde_json::Value) -> bool {
    v.as_array().is_some_and(|a| a.is_empty())
}

/// Parse `content` as JSON, run structural canonicalize in place, and return
/// the resulting [`serde_json::Value`].
///
/// Returns `None` when `content` is not a JSON object (same cases where
/// [`repair_query_plan_json`] returns the input unchanged).
///
/// This is the preferred entry for two-phase tests and for callers that
/// want a `Value` rather than a re-serialized string.
#[must_use]
pub fn canonicalize_to_value(content: &str) -> Option<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(content).ok()?;
    let obj = value.as_object_mut()?;
    let _changed = repair_query_plan_object(obj);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_to_value, repair_query_plan_json};
    use crate::parse::build::{from_canonical_str, from_canonical_value};
    use vlorql_core::schema::QueryPlan;

    // ------------------------------------------------------------------
    // Test helpers — two-phase pattern
    // ------------------------------------------------------------------
    //
    // Phase 1 (`check_canonical`):  raw input → canonical `Value`.
    //   Callers assert on the canonical shape (the "snapshot").
    //
    // Phase 2 (`check_build`):  canonical `Value` → typed `QueryPlan`.
    //   Verifies the canonical form actually deserializes.
    //
    // Both phases share the same canonical output, so they can be run
    // independently without repeating the canonicalize call.

    /// Phase 1: canonicalize `input` and assert both Value + string paths agree.
    /// Returns the canonical Value for further inspection.
    fn check_canonical(input: &str) -> serde_json::Value {
        let value = canonicalize_to_value(input)
            .unwrap_or_else(|| panic!("canonicalize_to_value returned None for: {input}"));
        // String path must agree with Value path (modulo key order / whitespace).
        let repaired = repair_query_plan_json(input);
        let from_str: serde_json::Value = serde_json::from_str(&repaired)
            .unwrap_or_else(|e| panic!("repaired string is not JSON: {e}; {repaired}"));
        assert_eq!(
            value, from_str,
            "canonicalize_to_value and repair_query_plan_json disagree"
        );
        value
    }

    /// Phase 2: build a typed `QueryPlan` from a canonical Value.
    /// Also asserts the string path produces the same plan.
    fn check_build(canonical: &serde_json::Value) -> QueryPlan {
        let plan = from_canonical_value(canonical)
            .unwrap_or_else(|e| panic!("build from Value failed: {e}; value={canonical}"));
        let via_str = from_canonical_str(&canonical.to_string())
            .unwrap_or_else(|e| panic!("build from str failed: {e}"));
        assert_eq!(plan, via_str);
        plan
    }

    /// Convenience: both phases in one call.
    #[track_caller]
    fn check_roundtrip(input: &str) -> (serde_json::Value, QueryPlan) {
        let canonical = check_canonical(input);
        let plan = check_build(&canonical);
        (canonical, plan)
    }

    #[test]
    fn canonical_moves_misplaced_fields_from_where_to_top_level() {
        // llama3.2 often puts order_by, limit, offset inside `where`
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"where":{"type":"and","left":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}],"limit":10}}"#;
        let parsed = check_canonical(input);
        assert!(
            parsed.get("order_by").is_some(),
            "order_by should be at top level, got: {parsed}"
        );
        assert!(
            parsed.get("limit").is_some(),
            "limit should be at top level, got: {parsed}"
        );
        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        assert!(
            wh.get("order_by").is_none(),
            "where should not have order_by"
        );
        assert!(wh.get("limit").is_none(), "where should not have limit");
    }

    #[test]
    fn canonical_unwraps_array_left_in_and_predicate() {
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"where":{"type":"and","left":[{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}],"right":{"type":"comparison","left":{"type":"column_ref","column":"status"},"op":"eq","right":{"type":"literal","value":"completed","data_type":"string"}}}}"#;
        let parsed = check_canonical(input);
        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        let left = wh.get("left").unwrap();
        assert!(
            left.is_object(),
            "left should be an object, not array: {left:?}"
        );
        assert_eq!(
            left.get("type").and_then(|t| t.as_str()),
            Some("comparison")
        );
    }

    #[test]
    fn canonical_does_not_modify_valid_query_plan() {
        let valid = r#"{"select":[{"type":"column_ref","table":"orders","column":"id"}],"from":{"table":"orders"},"where":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}],"limit":10}"#;
        let repaired = repair_query_plan_json(valid);
        assert!(
            matches!(repaired, std::borrow::Cow::Borrowed(_)),
            "valid JSON should not be modified"
        );
        // Phase 2: still builds
        check_build(&check_canonical(valid));
    }

    #[test]
    fn canonical_handles_llama3_2_output_with_multiple_issues() {
        let input = r#"{"select":[{"type":"column_ref","table":"orders","column":"id","alias":null},{"type":"column_ref","table":"users","column":"name","alias":null},{"type":"column_ref","table":"orders","column":"total","alias":null}], "from":{"table":"orders","alias":null}, "where":{"type":"and", "left":[{"type":"comparison","left":{"type":"column_ref","column":"total","table":"orders"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}],"group_by":[null], "having":{"type":"comparison","left":{"type":"column_ref","column":"total","table":"orders"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}},"order_by":[{"expr":{"type":"column_ref","column":"total","table":"orders"},"descending":false},{"expr":{"type":"column_ref","column":"id","table":"orders"},"descending":true}], "limit":10} }"#;
        let parsed = check_canonical(input);

        assert!(
            parsed.get("order_by").is_some(),
            "order_by should be at top level"
        );
        assert!(
            parsed.get("limit").is_some(),
            "limit should be at top level"
        );

        let wh = parsed.get("where").and_then(|w| w.as_object()).unwrap();
        assert!(
            wh.get("group_by").is_none(),
            "where should not have group_by"
        );
        assert!(wh.get("having").is_none(), "where should not have having");
        assert!(
            wh.get("order_by").is_none(),
            "where should not have order_by"
        );
        assert!(wh.get("limit").is_none(), "where should not have limit");

        let left = wh.get("left").unwrap();
        assert!(left.is_object(), "left should be object, got: {left:?}");

        check_build(&parsed);
    }

    #[test]
    fn canonical_handles_llama_join_with_missing_right_table() {
        let input = r#"{"from":{"alias":null,"table":"orders"},"select":[{"type":"column_ref","table":"orders","column":"id","alias":null},{"type":"column_ref","table":"users","column":"name","alias":null},{"type":"column_ref","table":"orders","column":"total","alias":null}], "joins":[{"join_type":"left","on":{"left":{"column":"users_id","table":"orders","type":"column_ref"},"op":"eq","right":{"column":"id","table":"users","type":"column_ref"},"type":"comparison"},"limit":10,"order_by":[{"descending":false,"expr":{"column":"total","table":"orders","type":"column_ref"}},{"descending":true,"expr":{"column":"id","table":"orders","type":"column_ref"}}],"where":{"left":{"column":"total","table":"orders","type":"column_ref"},"op":"gt","right":{"data_type":"float","type":"literal","value":150},"type":"comparison"}}]}"#;
        let parsed = check_canonical(input);

        assert!(
            parsed.get("where").is_some(),
            "where should be at top level"
        );
        assert!(
            parsed.get("order_by").is_some(),
            "order_by should be at top level"
        );
        assert!(
            parsed.get("limit").is_some(),
            "limit should be at top level"
        );

        let joins = parsed.get("joins").and_then(|j| j.as_array()).unwrap();
        assert_eq!(joins.len(), 1, "should have 1 join, not a duplicate");
        let rt = joins[0].get("right_table").unwrap();
        let rt_table = rt.get("table").and_then(|t| t.as_str()).unwrap();
        assert_eq!(
            rt_table, "users",
            "right_table should be 'users', got: {rt}"
        );

        let on = joins[0].get("on").and_then(|o| o.as_object()).unwrap();
        let left_col = on
            .get("left")
            .and_then(|l| l.as_object())
            .and_then(|l| l.get("column"))
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(left_col, "users_id", "LLM's original column name preserved");

        check_build(&parsed);
    }

    #[test]
    fn canonical_collapses_where_array_and_removes_invalid_select_items() {
        let input = r#"{"select":[{"type":"column_ref","column":"id","table":"orders"},{"type":"literal","value":150},{"type":"column_ref","column":"name","table":"users"},{"type":"column_ref","column":"total","table":"orders"}],"from":{"table":"orders"},"where":[{"type":"and","left":[{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150}}],"right":{"type":"literal","value":"active"}}],"order_by":[{"expr":{"type":"column_ref","column":"total"},"descending":true}]}"#;
        let parsed = check_canonical(input);

        let wh = parsed.get("where").unwrap();
        assert!(
            wh.is_object(),
            "where should be an object after repair, got: {wh:?}"
        );

        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        for item in select {
            let t = item.get("type").and_then(|t| t.as_str()).unwrap();
            assert!(
                ["column_ref", "expr", "star"].contains(&t),
                "select item has invalid type: {t}"
            );
        }
        assert_eq!(
            select.len(),
            3,
            "select should have 3 items after removing invalid one"
        );
    }

    #[test]
    fn canonical_injects_missing_type_on_join_on_and_select() {
        let input = r#"{
  "from": {"alias": null, "table": "orders"},
  "select": [
    {"alias": null, "table": "orders", "column": "id"},
    {"alias": null, "table": "users", "column": "name"},
    {"alias": null, "table": "orders", "column": "total"}
  ],
  "where": {
    "left": {"column": "total", "table": "orders", "type": "column_ref"},
    "op": "gt",
    "right": {"data_type": "float", "type": "literal", "value": 150},
    "type": "comparison"
  },
  "joins": [{
    "join_type": "inner",
    "on": {"table": "users", "column": "id", "table": "orders", "column": "user_id"},
    "right_table": {"alias": "u", "table": "users"},
    "left_table": {"alias": "o", "table": "orders"}
  }],
  "group_by": [null],
  "having": [null],
  "order_by": [{"descending": false, "expr": {"column": "total", "table": "orders", "type": "column_ref"}}],
  "limit": 10,
  "offset": 0
}"#;
        let parsed = check_canonical(input);
        check_build(&parsed);

        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        assert_eq!(select.len(), 3);
        for item in select {
            assert_eq!(
                item.get("type").and_then(|t| t.as_str()),
                Some("column_ref"),
                "select item should have type column_ref: {item:?}"
            );
        }

        let joins = parsed.get("joins").and_then(|j| j.as_array()).unwrap();
        assert_eq!(joins.len(), 1);
        let join = &joins[0];
        assert!(
            join.get("left_table").is_none(),
            "left_table should be stripped"
        );
        let on = join.get("on").and_then(|o| o.as_object()).unwrap();
        assert_eq!(
            on.get("type").and_then(|t| t.as_str()),
            Some("comparison"),
            "on should be wrapped as comparison predicate"
        );
        let left = on.get("left").and_then(|l| l.as_object()).unwrap();
        assert_eq!(
            left.get("type").and_then(|t| t.as_str()),
            Some("column_ref"),
            "on.left should have type column_ref: {left:?}"
        );
        let right = on.get("right").and_then(|r| r.as_object()).unwrap();
        assert_eq!(
            right.get("type").and_then(|t| t.as_str()),
            Some("column_ref"),
            "on.right should have type column_ref: {right:?}"
        );
        assert_eq!(
            right.get("table").and_then(|t| t.as_str()),
            Some("users"),
            "on.right.table should be the right_table name"
        );

        assert!(
            parsed.get("group_by").is_none(),
            "null group_by should be removed"
        );
    }

    #[test]
    fn canonical_injects_missing_type_on_bare_expression_in_group_by() {
        let input = r#"{
  "select": [{"type": "column_ref", "table": "orders", "column": "total"}],
  "from": {"table": "orders"},
  "group_by": [{"column": "status", "table": "orders"}],
  "order_by": [{"descending": false, "expr": {"column": "total", "table": "orders"}}]
}"#;
        let parsed = check_canonical(input);
        check_build(&parsed);

        let group_by = parsed.get("group_by").and_then(|g| g.as_array()).unwrap();
        assert_eq!(group_by.len(), 1);
        assert_eq!(
            group_by[0].get("type").and_then(|t| t.as_str()),
            Some("column_ref")
        );

        let order_by = parsed.get("order_by").and_then(|o| o.as_array()).unwrap();
        assert_eq!(order_by.len(), 1);
        assert_eq!(
            order_by[0]
                .get("expr")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str()),
            Some("column_ref")
        );
    }

    #[test]
    fn canonical_normalizes_data_type_variants() {
        let input = r#"{"select":[{"type":"star","table":"products"}],"from":{"table":"products","alias":null},"where":{"type":"not","child":{"type":"comparison","left":{"type":"column_ref","column":"id","table":"order_items"},"op":"eq","right":{"type":"literal","value":0,"data_type":"integer"}}}}"#;
        let parsed = check_canonical(input);

        let dt = parsed["where"]["child"]["right"]["data_type"]
            .as_str()
            .unwrap();
        assert_eq!(
            dt, "int",
            "data_type should be normalized from 'integer' to 'int', got: {dt}"
        );

        check_build(&parsed);
    }

    #[test]
    fn canonical_does_not_auto_join_missing_table() {
        // Auto-join inference was removed from canonicalize; column refs to
        // tables not in FROM/joins are passed through as-is to validation.
        let input = r#"{"from":{"table":"orders","alias":null},"select":[{"type":"column_ref","table":"orders","column":"id"},{"type":"column_ref","table":"users","column":"name"},{"type":"column_ref","table":"orders","column":"total"}]}"#;
        let (parsed, _plan) = check_roundtrip(input);
        assert!(
            parsed.get("joins").is_none(),
            "no auto-joins should be injected: {parsed}"
        );
    }

    #[test]
    fn canonical_does_not_auto_join_in_subquery() {
        let input = r#"{"from":{"table":"products","alias":null},"select":[{"type":"star"}],"where":{"type":"not","child":{"type":"exists","query":{"from":{"table":"order_items","alias":null},"select":[{"type":"star"}],"where":{"type":"comparison","left":{"type":"column_ref","table":"order_items","column":"product_id"},"op":"eq","right":{"type":"column_ref","table":"products","column":"id"}}}}}}"#;
        let (parsed, _plan) = check_roundtrip(input);
        let subquery = &parsed["where"]["child"]["query"];
        assert!(
            subquery.get("joins").is_none(),
            "no auto-joins should be injected in subquery: {subquery}"
        );
    }

    #[test]
    fn canonical_flattens_group_by_array_wrapper() {
        let input = r#"{"select":[{"alias":null,"table":"order_items","column":"quantity","type":"column_ref"}],"from":{"table":"products","alias":null},"where":{"left":{"type":"column_ref","column":"id","table":"order_items"},"op":"eq","right":{"data_type":"int","type":"literal","value":1}},"group_by":[{"items":[{"type":"column_ref","column":"id","table":"order_items"}],"type":"array"}]}"#;
        let parsed = check_canonical(input);

        let gb = parsed.get("group_by").and_then(|g| g.as_array()).unwrap();
        assert_eq!(gb.len(), 1, "should have 1 group_by item after flattening");
        assert_eq!(
            gb[0].get("type").and_then(|t| t.as_str()),
            Some("column_ref")
        );

        check_build(&parsed);
    }

    #[test]
    fn parse_query_plan_pipeline_matches_repair_then_serde() {
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"where":{"type":"and","left":[{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}],"right":{"type":"comparison","left":{"type":"column_ref","column":"status"},"op":"eq","right":{"type":"literal","value":"completed","data_type":"string"}}}}"#;
        let via_pipeline = crate::parse::parse_query_plan(input).expect("pipeline");
        let repaired = repair_query_plan_json(input);
        let via_legacy: QueryPlan = serde_json::from_str(&repaired).expect("legacy");
        assert_eq!(via_pipeline, via_legacy);
    }

    // ------------------------------------------------------------------
    // Edge-case smoke tests — each exercises a known LLM output quirk.
    // Phase 1 + Phase 2 must succeed end-to-end.
    // ------------------------------------------------------------------

    #[test]
    fn canonical_handles_qwen_join_corruption() {
        // Model concatenates fragments, producing a stray string inside
        // the `joins` array instead of a join object.
        // The raw content has literal tab characters after the stray string
        // (valid JSON whitespace outside the string).
        let input = "{\"from\":{\"table\":\"orders\"},\"joins\":[{\"join_type\":\"inner\",\"right_table\":{\"table\":\"users\"},\"on\":{\"left\":{\"column\":\"user_id\",\"table\":\"orders\",\"type\":\"column_ref\"},\"op\":\"eq\",\"right\":{\"column\":\"id\",\"table\":\"users\",\"type\":\"column_ref\"}}},{\"join_type\":\"inner\",\"right_table\":{\"table\":\"order_items\"},\"on\":{\"left\":{\"column\":\"id\",\"table\":\"orders\",\"type\":\"column_ref\"},\"op\":\"eq\",\"right\":{\"column\":\"order_id\",\"table\":\"order_items\",\"type\":\"column_ref\"}}},\"select':[{\"\t\t], \"from\": { \"table\": \"orders\" }, \"where\": {\"type\":\"comparison\",\"left\":{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"},\"op\":\"gt\",\"right\":{\"type\":\"literal\",\"value\":150,\"data_type\":\"float\"}},\"order_by\":[{\"expr\":{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"},\"descending\":true}],\"limit\":10}";
        let (parsed, _plan) = check_roundtrip(input);

        // Stray string filtered out; two valid joins remain.
        let joins = parsed.get("joins").and_then(|j| j.as_array()).unwrap();
        assert_eq!(joins.len(), 2, "should have 2 valid joins, got: {joins:?}");
        // select was missing (corrupted) → auto-injected star.
        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        assert_eq!(select[0].get("type").and_then(|t| t.as_str()), Some("star"));
    }

    #[test]
    fn canonical_handles_missing_select_with_from() {
        let input = r#"{"from":{"table":"orders"},"where":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}}"#;
        let (parsed, _plan) = check_roundtrip(input);
        let select = parsed.get("select").and_then(|s| s.as_array()).unwrap();
        assert_eq!(select.len(), 1);
        assert_eq!(select[0].get("type").and_then(|t| t.as_str()), Some("star"));
    }

    #[test]
    fn canonical_handles_where_as_array() {
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"users"},"where":[{"type":"comparison","left":{"type":"column_ref","column":"name"},"op":"eq","right":{"type":"literal","value":"alice","data_type":"string"}}]}"#;
        let (parsed, _plan) = check_roundtrip(input);
        let wh = parsed.get("where").unwrap();
        assert!(
            wh.is_object(),
            "where should be an object after canonicalize"
        );
    }

    #[test]
    fn canonical_removes_null_group_by() {
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"group_by":[null],"having":{"type":"comparison","left":{"type":"column_ref","column":"total"},"op":"gt","right":{"type":"literal","value":150,"data_type":"float"}}}"#;
        let (parsed, _plan) = check_roundtrip(input);
        assert!(
            parsed.get("group_by").is_none(),
            "null group_by should be removed"
        );
    }

    #[test]
    fn canonical_strips_top_level_descending_expr() {
        // Bare `descending`+`expr` at plan level are stripped by the
        // PLAN_FIELDS retain before step 5 can wrap them into order_by.
        // This is a known gap documented in the pipeline; the fields are
        // silently dropped.
        let input = r#"{"select":[{"type":"star"}],"from":{"table":"orders"},"descending":true,"expr":{"column":"total","table":"orders"}}"#;
        let (parsed, _plan) = check_roundtrip(input);
        assert!(
            parsed.get("order_by").is_none(),
            "order_by should be absent (step order bug: 1b strips descending before step 5)"
        );
        assert!(parsed.get("descending").is_none(), "descending stripped");
        assert!(parsed.get("expr").is_none(), "expr stripped");
        // Still builds as valid plan, order_by just missing.
        check_build(&parsed);
    }

    #[test]
    fn canonical_normalizes_field_aliases() {
        // filter → where, kind → type, col → column, operator → op, ...
        let (parsed, _plan) = check_roundtrip(
            r#"{"projection":[{"type":"star"}],"from":{"table_name":"users","alias_name":"u"},"filter":{"kind":"comparison","left":{"col":"name","type":"column_ref"},"operator":"eq","right":{"type":"literal","value":"alice","data_type":"string"}},"sort":[{"desc":true,"expr":{"col":"name","table_name":"users"}}],"limit_count":5}"#,
        );
        assert!(parsed.get("projection").is_none(), "projection → select");
        assert!(
            parsed.get("select").is_some(),
            "select present after rename"
        );
        assert!(parsed.get("filter").is_none(), "filter → where");
        let wh = parsed["where"].as_object().unwrap();
        assert_eq!(
            wh.get("type").and_then(|t| t.as_str()),
            Some("comparison"),
            "kind → type"
        );
        assert!(wh.get("operator").is_none(), "operator → op");
        assert!(parsed.get("sort").is_none(), "sort → order_by");
        assert!(
            parsed.get("order_by").is_some(),
            "order_by present after rename"
        );
        assert!(parsed.get("limit_count").is_none(), "limit_count → limit");
        assert_eq!(parsed.get("limit").and_then(|l| l.as_i64()), Some(5));
        // Recursive rename: col → column inside expr inside order_by.
        let ob = parsed["order_by"].as_array().unwrap();
        let expr = ob[0]["expr"].as_object().unwrap();
        assert_eq!(expr.get("column").and_then(|c| c.as_str()), Some("name"));
        assert!(expr.get("col").is_none());
    }
}
