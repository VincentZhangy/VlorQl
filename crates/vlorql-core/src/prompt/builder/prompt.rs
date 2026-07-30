use std::fmt::Write;
use std::hash::Hasher;

use crate::cache::{PromptCache, PromptCacheKey};
use crate::prompt::PromptSkill;
use crate::schema::{JoinType, SqlDialect};
use xxhash_rust::xxh3::Xxh3;

use super::PromptBuilder;

impl PromptBuilder {
    /// Builds the complete system prompt for one user question.
    pub async fn build_system_prompt(&self, user_question: &str) -> String {
        let relevant_tables = self.filter_relevant_tables(user_question).await;
        self.build_system_prompt_for_tables(&relevant_tables)
    }

    fn build_system_prompt_for_tables(&self, relevant_tables: &[String]) -> String {
        let mut prompt = String::new();

        prompt.push_str(
            "# Role\n\
             You are an SQL query planner. Given the user question and schema below, output a JSON query plan. Raw SQL is forbidden.\n\
             \n",
        );
        self.push_schema_description(&mut prompt, relevant_tables);
        if let Some(ref skill) = self.skill {
            self.push_skill_instructions(&mut prompt, skill);
        }
        self.push_dialect_constraints(&mut prompt);
        self.push_planning_rules(&mut prompt);
        self.push_output_schema(&mut prompt);
        self.push_type_guidance(&mut prompt);
        if self.include_examples {
            if let Some(ref skill) = self.skill {
                if !skill.examples.is_empty() {
                    self.push_skill_examples(&mut prompt, skill);
                } else {
                    self.push_example(&mut prompt, relevant_tables);
                }
            } else {
                self.push_example(&mut prompt, relevant_tables);
            }
        }

        prompt
    }

    /// Builds the system prompt with cache support.
    pub async fn build_system_prompt_with_cache(
        &self,
        user_question: &str,
        cache: &PromptCache,
    ) -> String {
        let schema_version = self.schema.metadata.version.as_deref().unwrap_or("unknown");

        let relevant_tables = self.filter_relevant_tables(user_question).await;

        let mut hasher = Xxh3::new();
        for table in &relevant_tables {
            ::std::hash::Hash::hash(table, &mut hasher);
        }
        let table_hash = hasher.finish();

        let key = PromptCacheKey::new(schema_version, &self.dialect, self.policy_hash, table_hash);

        if let Some(cached) = cache.get(&key).await {
            return cached;
        }

        let prompt = self.build_system_prompt_for_tables(&relevant_tables);

        cache.insert(key, prompt.clone()).await;

        prompt
    }

    fn push_schema_description(&self, prompt: &mut String, relevant_tables: &[String]) {
        prompt.push_str("## Schema\n");

        let mut has_visible = false;
        for table_name in relevant_tables {
            let Some(table) = self.schema.get_table(table_name) else {
                continue;
            };
            let policy = self.policy.table_policies.get(&table.name);
            if policy.is_some_and(|p| !p.allowed) {
                continue;
            }

            let cols: Vec<String> = table
                .columns
                .iter()
                .filter(|c| self.column_visible(table, c, policy))
                .map(|c| {
                    let mut desc = format!("{} {}", c.name, c.data_type.type_name());
                    if let Some(ref fk) = c.foreign_key {
                        let _ = write!(desc, " → {}.{}", fk.foreign_table, fk.foreign_column);
                    }
                    desc
                })
                .collect();

            if cols.is_empty() {
                continue;
            }

            let _ = writeln!(prompt, "{}({})", table.name, cols.join(", "));
            has_visible = true;
        }

        if !has_visible {
            prompt.push_str("(none available)\n");
        }
        prompt.push('\n');

        // ── Relationships section ────────────────────────────────
        let mut rels: Vec<String> = Vec::new();
        for table_name in relevant_tables {
            let Some(table) = self.schema.get_table(table_name) else {
                continue;
            };
            let policy = self.policy.table_policies.get(&table.name);
            if policy.is_some_and(|p| !p.allowed) {
                continue;
            }
            for column in &table.columns {
                if let Some(ref fk) = column.foreign_key
                    && relevant_tables.contains(&fk.foreign_table)
                {
                    rels.push(format!(
                        "{}.{} → {}.{}",
                        table.name, column.name, fk.foreign_table, fk.foreign_column
                    ));
                }
            }
        }
        if !rels.is_empty() {
            prompt.push_str("## Relationships\n");
            for rel in &rels {
                let _ = writeln!(prompt, "{rel}");
            }
            prompt.push('\n');
        }
        prompt.push_str("Remember: referencing `table.column` in SELECT without joining `table` first is invalid.\n\n");
    }

    fn push_dialect_constraints(&self, prompt: &mut String) {
        let dialect_name = sql_dialect_name(self.dialect.dialect);

        let feature_flags: Vec<String> = [
            ("ctes", self.dialect.supports_cte),
            ("window_functions", self.dialect.supports_window_functions),
            ("json_operations", self.dialect.supports_json_operations),
            ("distinct", self.dialect.allow_distinct),
            ("offset", self.dialect.supports_offset),
            ("fetch", self.dialect.supports_fetch),
        ]
        .iter()
        .map(|(name, enabled)| {
            let enabled = *enabled && !self.is_forbidden_by_skill(name);
            if enabled {
                format!("+{name}")
            } else {
                format!("-{name}")
            }
        })
        .collect();

        let join_types: Vec<&str> = self
            .dialect
            .allowed_join_types
            .iter()
            .map(join_type_name)
            .collect();
        let joins = if join_types.is_empty() {
            "none".to_owned()
        } else {
            format!(
                "{} (max {})",
                join_types.join(", "),
                optional_limit(self.dialect.max_joins)
            )
        };

        let func_allow = if self.dialect.allowed_functions.is_empty() {
            "unrestricted".to_owned()
        } else {
            format!("allowlist: {}", self.dialect.allowed_functions.join(", "))
        };
        let func_deny = if self.dialect.denied_functions.is_empty() {
            "none".to_owned()
        } else {
            format!("denylist: {}", self.dialect.denied_functions.join(", "))
        };

        let _ = writeln!(prompt, "## Dialect");
        let _ = writeln!(prompt, "Dialect: {dialect_name}");
        let _ = writeln!(prompt, "Features: {}", feature_flags.join(", "));
        let _ = writeln!(prompt, "Joins: {joins}");
        let _ = writeln!(prompt, "Functions: {func_allow} | {func_deny}");
        let _ = writeln!(
            prompt,
            "GroupBy: {}\n",
            optional_limit(self.dialect.max_group_by_columns)
        );
    }

    fn push_skill_instructions(&self, prompt: &mut String, skill: &PromptSkill) {
        if skill.instructions.is_empty() {
            return;
        }
        prompt.push_str("## Skill Instructions\n");
        for instruction in &skill.instructions {
            let _ = writeln!(prompt, "- {instruction}");
        }
        prompt.push('\n');
    }

    fn push_skill_examples(&self, prompt: &mut String, skill: &PromptSkill) {
        prompt.push_str("## Examples\n");
        for example in &skill.examples {
            let plan_str = serde_json::to_string(&example.plan).unwrap_or_default();
            let _ = writeln!(prompt, "Q: {}\nA: {plan_str}\n", example.question);
        }
        let _ = writeln!(
            prompt,
            "The real response must obey the current schema and dialect.\n",
        );
    }

    fn push_planning_rules(&self, prompt: &mut String) {
        prompt.push_str(
            "## Core Rules\n\
             ### JSON Structure Rules (MUST follow)\n\
             1. Output ONLY a valid JSON object — no markdown fences, no comments, no raw SQL, no trailing text.\n\
             2. Every key and string value MUST use double quotes (`\"`). Single quotes (`'`) are INVALID JSON.\n\
             3. Every tagged object MUST include a `\"type\"` field matching the JSON Schema below.\n\
             4. `where`, `having`, `left`, `right`, `child`, `on` must each be a single Predicate object `{...}`, NEVER an array `[...]`.\n\
             5. `order_by`, `limit`, `offset`, `group_by`, `having`, `joins`, `ctes` ONLY at the top level, NEVER inside `where`.\n\
             6. Every table in `select`/`where` MUST be in `from` or `joins`. Never reference an unjoined table.\n\
             \n\
             ### Column Name Rules (MUST follow)\n\
             7. Use EXACT column names from the Schema section. Never invent, guess, or rename columns.\n\
             8. NEVER embed SQL in a column name string. `{\"column\":\"EXTRACT(MONTH FROM created_at)\"}` is WRONG.\n\
             \n\
             ### Query Logic Rules\n\
             9. Prefer the simplest plan. Fewer joins, shallower nesting.\n\
             10. NEVER add JOINs when the question only involves one table.\n\
             11. Only use GROUP BY when the question asks for \"each / every / per\".\n\
             12. Aggregates like `sum`/`count` go in HAVING, not WHERE.\n\
             13. \"never / not / without / anti-join\" questions: use LEFT JOIN + `is_null`. NEVER use `NOT EXISTS`.\n\
             14. \"all combinations / every pairing / cartesian product\": use CROSS JOIN with no `on`.\n\
             \n",
        );
    }

    fn push_output_schema(&self, prompt: &mut String) {
        prompt.push_str(
            "## Required JSON Output\n\
             Structure:\n\
             - select: [Projection] (type: column_ref → {table?, column, alias?} | expr → {expression, alias?} | star → {table?})\n\
             - from: {table, alias?}\n\
             - where: optional Predicate\n\
             - joins: optional [{join_type, right_table: FromClause, on: Predicate}]\n\
             - group_by: optional [Expression] | having: optional Predicate\n\
             - order_by: optional [{expr: Expression, descending: bool}]\n\
             - limit, offset: optional integer | ctes: optional [{name, query: QueryPlan}]\n\
             \n\
             CRITICAL: `where`, `left`, `right`, `child` must each be a SINGLE Predicate object (NOT an array).\n\
             CRITICAL: `order_by`, `limit`, `offset` go at the top level of the response, never inside `where`.\n\
             CRITICAL: Emit parseable JSON only — never escape object keys, never wrap the whole plan in a string, never invent trailing `}`.\n\
             Use the tagged type variants. Return a data instance — not a schema definition.\n\
             Output JSON only: no fences, comments, or raw SQL.\n\
             \n\
             Compact JSON Schema (every field is inlined, no $ref):\n\
             ```json\n",
        );
        prompt.push_str(compact_output_schema());
        prompt.push_str("\n```\n\n");
    }

    fn push_example(&self, prompt: &mut String, relevant_tables: &[String]) {
        let example = relevant_tables.iter().find_map(|table_name| {
            let table = self.schema.get_table(table_name)?;
            let policy = self.policy.table_policies.get(table_name);
            if policy.is_some_and(|policy| !policy.allowed) {
                return None;
            }
            let column = table
                .columns
                .iter()
                .find(|column| self.column_visible(table, column, policy))?;
            Some((table, column))
        });
        let Some((table, column)) = example else {
            return;
        };

        let example_json = serde_json::json!({
            "select": [{
                "type": "column_ref",
                "table": table.name,
                "column": column.name,
                "alias": null
            }],
            "from": {
                "table": table.name,
                "alias": null
            }
        });
        let _ = writeln!(
            prompt,
            "## Example\n\
             Q: Select {} from {}\n\
             A: {example_json}\n",
            column.name, table.name
        );
        let _ = writeln!(
            prompt,
            "Q: Orders with total > 150, sorted by total desc\n\
              A: {{\"select\":[{{\"type\":\"column_ref\",\"table\":\"orders\",\"column\":\"id\",\"alias\":null}},{{\"type\":\"column_ref\",\"table\":\"orders\",\"column\":\"total\",\"alias\":null}}],\"from\":{{\"table\":\"orders\",\"alias\":null}},\"where\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"}},\"op\":\"gt\",\"right\":{{\"type\":\"literal\",\"value\":150,\"data_type\":\"float\"}}}},\"order_by\":[{{\"expr\":{{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"}},\"descending\":true}}],\"limit\":10}}\n"
        );
        let has_join_example = relevant_tables.len() >= 2
            && relevant_tables.iter().any(|t1| {
                self.schema.get_table(t1).is_some_and(|table| {
                    table.columns.iter().any(|c| {
                        c.foreign_key
                            .as_ref()
                            .is_some_and(|fk| relevant_tables.contains(&fk.foreign_table))
                    })
                })
            });
        if has_join_example {
            let _ = writeln!(
                prompt,
                "Q: List users with their order totals\n\
                  A: {{\"select\":[{{\"type\":\"column_ref\",\"table\":\"users\",\"column\":\"name\",\"alias\":null}},{{\"type\":\"column_ref\",\"table\":\"orders\",\"column\":\"total\",\"alias\":null}}],\"from\":{{\"table\":\"users\",\"alias\":null}},\"joins\":[{{\"join_type\":\"inner\",\"right_table\":{{\"table\":\"orders\",\"alias\":null}},\"on\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"id\",\"table\":\"users\"}},\"op\":\"eq\",\"right\":{{\"type\":\"column_ref\",\"column\":\"user_id\",\"table\":\"orders\"}}}}}}],\"limit\":10}}\n"
            );
            let _ = writeln!(
                prompt,
                "Q: Users who never placed an order\n\
                  A: {{\"select\":[{{\"type\":\"column_ref\",\"table\":\"users\",\"column\":\"id\",\"alias\":null}},{{\"type\":\"column_ref\",\"table\":\"users\",\"column\":\"name\",\"alias\":null}}],\"from\":{{\"table\":\"users\",\"alias\":null}},\"joins\":[{{\"join_type\":\"left\",\"right_table\":{{\"table\":\"orders\",\"alias\":null}},\"on\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"id\",\"table\":\"users\"}},\"op\":\"eq\",\"right\":{{\"type\":\"column_ref\",\"column\":\"user_id\",\"table\":\"orders\"}}}}}}],\"where\":{{\"type\":\"is_null\",\"expr\":{{\"type\":\"column_ref\",\"column\":\"id\",\"table\":\"orders\"}}}},\"limit\":10}}\n"
            );
            let _ = writeln!(
                prompt,
                "Q: Users with no orders (NOT EXISTS form)\n\
                  A: {{\"select\":[{{\"type\":\"column_ref\",\"table\":\"users\",\"column\":\"id\",\"alias\":null}}],\"from\":{{\"table\":\"users\",\"alias\":null}},\"where\":{{\"type\":\"not\",\"child\":{{\"type\":\"exists\",\"query\":{{\"select\":[{{\"type\":\"star\"}}],\"from\":{{\"table\":\"orders\",\"alias\":null}},\"where\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"user_id\",\"table\":\"orders\"}},\"op\":\"eq\",\"right\":{{\"type\":\"column_ref\",\"column\":\"id\",\"table\":\"users\"}}}}}}}}}},\"limit\":10}}\n"
            );
            let _ = writeln!(
                prompt,
                "Q: How many items were sold per product?\n\
                  A: {{\"select\":[{{\"type\":\"column_ref\",\"table\":\"products\",\"column\":\"name\",\"alias\":\"product\"}},{{\"type\":\"expr\",\"expression\":{{\"type\":\"function_call\",\"name\":\"sum\",\"args\":[{{\"type\":\"column_ref\",\"column\":\"quantity\",\"table\":\"order_items\"}}],\"distinct\":false}},\"alias\":\"total_sold\"}}],\"from\":{{\"table\":\"products\",\"alias\":null}},\"joins\":[{{\"join_type\":\"inner\",\"right_table\":{{\"table\":\"order_items\",\"alias\":null}},\"on\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"id\",\"table\":\"products\"}},\"op\":\"eq\",\"right\":{{\"type\":\"column_ref\",\"column\":\"product_id\",\"table\":\"order_items\"}}}}}}],\"group_by\":[{{\"type\":\"column_ref\",\"table\":\"products\",\"column\":\"name\"}}]}}\n"
            );
        }
        let _ = writeln!(
            prompt,
            "\n\
              The real response must obey the current schema and dialect.\n",
        );
    }

    fn push_type_guidance(&self, prompt: &mut String) {
        prompt.push_str(
            "## JSON Field Reference\n\
             \n\
             ### Predicate types (where / having / on / left / right / child)\n\
             Valid `\"type\"` values: `\"comparison\"`, `\"and\"`, `\"or\"`, `\"not\"`, `\"between\"`, `\"in\"`, `\"like\"`, `\"is_null\"`, `\"exists\"`.\n\
             NEVER use aggregate names (`count`, `sum`, ...) or expression types (`column_ref`, `literal`, ...) as predicate type.\n\
             \n\
             | Type | Fields |\n\
             |------|-------|\n\
             | `comparison` | `left`: Expression, `op`: string (eq/neq/gt/gte/lt/lte), `right`: Expression |\n\
             | `and` / `or` | `left`: Predicate, `right`: Predicate |\n\
             | `not` | `child`: Predicate |\n\
             | `between` | `expr`: Expression, `low`: Expression, `high`: Expression |\n\
             | `in` | `expr`: Expression, `target`: [Expression] or QueryPlan |\n\
             | `like` | `expr`: Expression, `pattern`: string |\n\
             | `is_null` | `expr`: Expression |\n\
             | `exists` | `query`: QueryPlan |\n\
             \n\
             ### Expression types\n\
             Valid `\"type\"` values: `\"column_ref\"`, `\"literal\"`, `\"function_call\"`, `\"binary_op\"`, `\"star\"`, `\"subquery\"`, `\"case\"`, `\"window_function\"`.\n\
             \n\
             | Type | Fields |\n\
             |------|-------|\n\
             | `column_ref` | `table?`: string, `column`: string |\n\
             | `literal` | `value`: any, `data_type`: string (int/float/string/boolean/null/decimal/date/timestamp/json) |\n\
             | `function_call` | `name`: string, `args`: [Expression], `distinct?`: bool |\n\
             | `binary_op` | `left`: Expression, `op`: string, `right`: Expression |\n\
             \n\
             ### Projection types (inside select[])\n\
             | Type | Fields |\n\
             |------|-------|\n\
             | `column_ref` | `table?`: string, `column`: string, `alias?`: string |\n\
             | `expr` | `expression`: Expression, `alias?`: string |\n\
             | `star` | `table?`: string |\n\
             \n\
             ### FROM / JOIN types\n\
             - `from`: `{\"table\": string, \"alias\"?: string}`\n\
             - `joins[]`: each entry = `{\"join_type\": string, \"right_table\": FromClause, \"on\": Predicate}`\n\
             \n\
             ## Common Mistakes to Avoid\n\
             \n\
             1. NEVER put aggregate names as predicate `\"type\"` — `{\"type\":\"count\",...}` is WRONG, use `{\"type\":\"function_call\",\"name\":\"count\",...}`.\n\
             2. NEVER embed SQL in column names — `{\"column\":\"EXTRACT(MONTH FROM created_at)\"}` is WRONG.\n\
             3. NEVER invent column names — use the EXACT names from the Schema section.\n\
             4. NEVER use single quotes (`'`) — only double quotes (`\"`) are valid JSON.\n\
             5. NEVER output markdown fences, trailing backticks, or raw SQL.\n\
             6. NEVER nest aggregates: `SUM(SUM(x))` → write `SUM(x)` once.\n\
             7. NEVER put `data_type` on anything other than a `literal` object.\n\
             8. NEVER add JOINs when a single table is sufficient.\n\
             \n",
        );
    }
}

fn sql_dialect_name(dialect: SqlDialect) -> &'static str {
    match dialect {
        SqlDialect::Postgres => "postgres",
        SqlDialect::Sqlite => "sqlite",
        SqlDialect::MySql => "mysql",
    }
}

fn join_type_name(join_type: &JoinType) -> &'static str {
    match join_type {
        JoinType::Inner => "inner",
        JoinType::Left => "left",
        JoinType::Right => "right",
        JoinType::Full => "full",
        JoinType::Cross => "cross",
    }
}

fn optional_limit(limit: Option<usize>) -> String {
    limit.map_or_else(|| "no explicit limit".to_owned(), |limit| limit.to_string())
}

fn compact_output_schema() -> &'static str {
    use std::sync::OnceLock;
    static SCHEMA: OnceLock<String> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::to_string(&serde_json::json!({
            "type": "object",
            "properties": {
                "select": {
                    "type": "array",
                    "items": {
                        "oneOf": [
                            {"type": "object", "properties": {"type": {"enum": ["column_ref"]}, "table": {"type": "string"}, "column": {"type": "string"}, "alias": {"type": "string"}}, "required": ["type", "column"]},
                            {"type": "object", "properties": {"type": {"enum": ["expr"]}, "expression": {"type": "object"}, "alias": {"type": "string"}}, "required": ["type", "expression"]},
                            {"type": "object", "properties": {"type": {"enum": ["star"]}, "table": {"type": "string"}}, "required": ["type"]}
                        ]
                    }
                },
                "distinct": {"type": "boolean"},
                "distinct_on": {"type": "array", "items": {"type": "object"}},
                "from": {"type": "object", "properties": {"table": {"type": "string"}, "alias": {"type": "string"}}, "required": ["table"]},
                "where": {
                    "oneOf": [
                        {"type": "object", "properties": {"type": {"enum": ["comparison"]}, "left": {"type": "object"}, "op": {"enum": ["eq","neq","gt","gte","lt","lte","like","ilike","in","between"]}, "right": {"type": "object"}}, "required": ["type","left","op","right"]},
                        {"type": "object", "properties": {"type": {"enum": ["and"]}, "left": {"type": "object"}, "right": {"type": "object"}}, "required": ["type","left","right"]},
                        {"type": "object", "properties": {"type": {"enum": ["or"]}, "left": {"type": "object"}, "right": {"type": "object"}}, "required": ["type","left","right"]},
                        {"type": "object", "properties": {"type": {"enum": ["not"]}, "child": {"type": "object"}}, "required": ["type","child"]},
                        {"type": "object", "properties": {"type": {"enum": ["between"]}, "expr": {"type": "object"}, "low": {"type": "object"}, "high": {"type": "object"}}, "required": ["type","expr","low","high"]},
                        {"type": "object", "properties": {"type": {"enum": ["in"]}, "expr": {"type": "object"}, "target": {"type": "array","items": {"type": "object"}}}, "required": ["type","expr","target"]},
                        {"type": "object", "properties": {"type": {"enum": ["like"]}, "expr": {"type": "object"}, "pattern": {"type": "string"}}, "required": ["type","expr","pattern"]},
                        {"type": "object", "properties": {"type": {"enum": ["is_null"]}, "expr": {"type": "object"}}, "required": ["type","expr"]},
                        {"type": "object", "properties": {"type": {"enum": ["exists"]}, "query": {"type": "object"}}, "required": ["type","query"]}
                    ]
                },
                "joins": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "join_type": {"enum": ["inner","left","right","full","cross"]},
                            "right_table": {"type": "object", "properties": {"table": {"type": "string"}, "alias": {"type": "string"}}, "required": ["table"]},
                            "on": {"type": "object"}
                        },
                        "required": ["join_type","right_table","on"]
                    }
                },
                "group_by": {"type": "array", "items": {"type": "object"}},
                "having": {"type": "object"},
                "order_by": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"expr": {"type": "object"}, "descending": {"type": "boolean"}},
                        "required": ["expr","descending"]
                    }
                },
                "limit": {"type": "integer"},
                "offset": {"type": "integer"},
                "ctes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "recursive": {"type": "boolean"},
                            "query": {"type": "object"}
                        },
                        "required": ["name","query"]
                    }
                },
                "set_operation": {
                    "type": "object",
                    "properties": {
                        "operation": {"enum": ["union_all","union","intersect","except"]},
                        "right": {"type": "object"}
                    },
                    "required": ["operation","right"]
                }
            },
            "required": ["select", "from"]
        }))
        .expect("compact output schema must serialize to JSON")
    })
}
