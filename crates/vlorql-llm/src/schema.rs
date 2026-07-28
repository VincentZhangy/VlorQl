use serde_json::{Value, json};
use std::sync::OnceLock;

pub(crate) fn compact_query_plan_schema() -> Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(simplified_query_plan_schema).clone()
}

/// Returns a flattened JSON Schema for `QueryPlan` without `$ref` or `$defs`.
///
/// The auto-generated schema from `schemars::schema_for!(QueryPlan)` contains
/// recursive `$ref` definitions that confuse smaller Ollama models (4B–7B),
/// causing them to output schema fragments instead of actual data. This
/// manually constructed schema inlines everything so the model can produce
/// valid output directly.
fn simplified_query_plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "select": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string", "enum": ["column_ref", "expr", "star"] },
                        "table": { "type": "string" },
                        "column": { "type": "string" },
                        "expression": { "$ref": "#/definitions/Expression" },
                        "alias": { "type": "string" }
                    },
                    "required": ["type"]
                }
            },
            "from": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "alias": { "type": "string" }
                },
                "required": ["table"]
            },
            "where": { "$ref": "#/definitions/Predicate" },
            "group_by": {
                "type": "array",
                "items": { "$ref": "#/definitions/Expression" }
            },
            "having": { "$ref": "#/definitions/Predicate" },
            "order_by": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "expr": { "$ref": "#/definitions/Expression" },
                        "descending": { "type": "boolean" }
                    },
                    "required": ["expr", "descending"]
                }
            },
            "limit": { "type": "integer" },
            "offset": { "type": "integer" },
            "joins": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "join_type": { "type": "string", "enum": ["inner", "left", "right", "full", "cross"] },
                        "right_table": {
                            "type": "object",
                            "properties": {
                                "table": { "type": "string" },
                                "alias": { "type": "string" }
                            },
                            "required": ["table"]
                        },
                        "on": { "$ref": "#/definitions/Predicate" }
                    },
                    "required": ["join_type", "right_table", "on"]
                }
            },
            "ctes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "query": { "$ref": "#/definitions/QueryPlan" }
                    },
                    "required": ["name", "query"]
                }
            }
        },
        "required": ["select", "from"],
        "definitions": {
            "QueryPlan": {
                "type": "object",
                "properties": {
                    "select": { "type": "array", "items": { "type": "object" } },
                    "from": { "type": "object", "properties": { "table": { "type": "string" }, "alias": { "type": "string" } } },
                    "where": { "type": "object" },
                    "group_by": { "type": "array", "items": { "type": "object" } },
                    "having": { "type": "object" },
                    "order_by": { "type": "array", "items": { "type": "object" } },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" },
                    "joins": { "type": "array", "items": { "type": "object" } },
                    "ctes": { "type": "array", "items": { "type": "object" } }
                }
            },
            "Expression": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["literal"] },
                            "value": { "type": ["string", "number", "boolean", "null"] },
                            "data_type": { "type": "string", "enum": ["int", "float", "string", "boolean", "date", "timestamp", "json", "uuid", "decimal", "array", "jsonb", "blob"] }
                        },
                        "required": ["type", "value", "data_type"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["column_ref"] },
                            "table": { "type": "string" },
                            "column": { "type": "string" }
                        },
                        "required": ["type", "column"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["function_call"] },
                            "name": { "type": "string" },
                            "args": { "type": "array", "items": { "type": "object" } },
                            "distinct": { "type": "boolean" }
                        },
                        "required": ["type", "name", "args"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["binary_op"] },
                            "left": { "type": "object" },
                            "op": { "type": "string", "enum": ["add", "sub", "mul", "div", "and", "or"] },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "op", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["star"] }
                        },
                        "required": ["type"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["sub_query"] },
                            "query": { "type": "object" }
                        },
                        "required": ["type", "query"]
                    }
                ]
            },
            "Predicate": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["comparison"] },
                            "left": { "type": "object" },
                            "op": { "type": "string", "enum": ["eq", "neq", "gt", "gte", "lt", "lte", "like", "not_like", "is", "is_not"] },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "op", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["and"] },
                            "left": { "type": "object" },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["or"] },
                            "left": { "type": "object" },
                            "right": { "type": "object" }
                        },
                        "required": ["type", "left", "right"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["not"] },
                            "child": { "type": "object" }
                        },
                        "required": ["type", "child"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["between"] },
                            "expr": { "type": "object" },
                            "low": { "type": "object" },
                            "high": { "type": "object" }
                        },
                        "required": ["type", "expr", "low", "high"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["in"] },
                            "expr": { "type": "object" },
                            "target": {
                                "oneOf": [
                                    { "type": "array", "items": { "type": "object" } },
                                    { "type": "object" }
                                ]
                            }
                        },
                        "required": ["type", "expr", "target"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["like"] },
                            "expr": { "type": "object" },
                            "pattern": { "type": "string" }
                        },
                        "required": ["type", "expr", "pattern"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["is_null"] },
                            "expr": { "type": "object" }
                        },
                        "required": ["type", "expr"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "type": { "type": "string", "enum": ["exists"] },
                            "query": { "type": "object" }
                        },
                        "required": ["type", "query"]
                    }
                ]
            }
        }
    })
}
