//! Compact system prompt construction with DDL schema and minimal dialect constraints.

use crate::cache::{PromptCache, PromptCacheKey, hash_policy};
use crate::policy::{PolicyConfig, TablePolicy};
use crate::schema::{
    ColumnSchema, DataType, DialectProfile, JoinType, QueryPlan, SchemaSnapshot, SqlDialect,
    TableSchema,
};
use schemars::schema_for;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// Builds strict LLM instructions from a shared schema, dialect, and policy.
///
/// # Examples
///
/// ```
/// use vlorql_core::prompt::PromptBuilder;
/// use vlorql_core::schema::{SchemaSnapshot, DialectProfile, SqlDialect, TableSchema, ColumnSchema, DataType, SchemaMetadata};
/// use vlorql_core::policy::PolicyConfig;
/// use std::sync::Arc;
///
/// let schema = Arc::new(SchemaSnapshot::new(
///     vec![TableSchema {
///         name: "users".to_owned(),
///         columns: vec![ColumnSchema {
///             name: "id".to_owned(), data_type: DataType::Int,
///             nullable: false, description: None,
///             is_primary_key: true, foreign_key: None,
///         }],
///         description: None, primary_key: Some(vec!["id".to_owned()]),
///     }],
///     SchemaMetadata::default(),
/// ));
/// let builder = PromptBuilder::new(
///     schema,
///     DialectProfile::default(),
///     PolicyConfig::default(),
/// );
/// let prompt = builder.build_system_prompt("Show me users");
/// assert!(prompt.contains("users"));
/// assert!(prompt.contains("query plan"));
/// ```
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    schema: Arc<SchemaSnapshot>,
    dialect: DialectProfile,
    policy: PolicyConfig,
    /// Pre-computed hash of the policy configuration, used in cache keys.
    policy_hash: u64,
    include_examples: bool,
    /// Reverse foreign-key index: maps foreign_table → list of local tables
    /// that reference it. Built once in [`Self::new`] to avoid O(n²) traversal
    /// in [`Self::expand_foreign_key_neighbors`].
    reverse_fk_index: HashMap<String, Vec<String>>,
}

impl PromptBuilder {
    /// Creates a prompt builder that includes one compact example by default.
    pub fn new(schema: Arc<SchemaSnapshot>, dialect: DialectProfile, policy: PolicyConfig) -> Self {
        let reverse_fk_index = build_reverse_fk_index(&schema);
        Self {
            schema,
            dialect,
            policy_hash: hash_policy(&policy),
            policy,
            include_examples: true,
            reverse_fk_index,
        }
    }

    /// Enables or disables the optional example section.
    #[must_use]
    pub fn with_examples(mut self, include_examples: bool) -> Self {
        self.include_examples = include_examples;
        self
    }

    /// Builds the complete system prompt for one user question.
    ///
    /// The question is used only for schema retrieval and is deliberately not copied
    /// into the system prompt, preventing user text from becoming system instructions.
    pub fn build_system_prompt(&self, user_question: &str) -> String {
        let relevant_tables = self.filter_relevant_tables(user_question);
        self.build_system_prompt_for_tables(&relevant_tables)
    }

    /// Builds the system prompt using an explicitly provided table list.
    ///
    /// This is the inner implementation shared by [`Self::build_system_prompt`]
    /// and [`Self::build_system_prompt_with_cache`] so that the table set
    /// can be computed once and reused for both cache-key generation and
    /// prompt construction.
    fn build_system_prompt_for_tables(&self, relevant_tables: &[String]) -> String {
        let mut prompt = String::new();

        prompt.push_str(
            "# Role\n\
             You are an SQL query planner. Given the user question and schema below, output a JSON query plan. Raw SQL is forbidden.\n\
             \n",
        );
        self.push_schema_description(&mut prompt, relevant_tables);
        self.push_dialect_constraints(&mut prompt);
        self.push_output_schema(&mut prompt);
        self.push_type_guidance(&mut prompt);
        if self.include_examples {
            self.push_example(&mut prompt, relevant_tables);
        }

        prompt
    }

    /// Builds the system prompt with cache support.
    ///
    /// When a cache is provided and the key is present, the cached prompt
    /// is returned without re-building.  On a cache miss the prompt is
    /// generated and inserted into the cache before returning.
    ///
    /// This method is async because the cache uses an async backing store.
    pub async fn build_system_prompt_with_cache(
        &self,
        user_question: &str,
        cache: &PromptCache,
    ) -> String {
        let schema_version = self.schema.metadata.version.as_deref().unwrap_or("unknown");

        // Compute the relevant table set first so it can be included
        // in the cache key — different questions that match different
        // tables must produce different cache entries.
        let relevant_tables = self.filter_relevant_tables(user_question);

        // Hash the relevant table names for the cache key.
        let mut hasher = Xxh3::new();
        for table in &relevant_tables {
            table.hash(&mut hasher);
        }
        let table_hash = hasher.finish();

        let key = PromptCacheKey::new(schema_version, &self.dialect, self.policy_hash, table_hash);

        // Try cache hit.
        if let Some(cached) = cache.get(&key).await {
            return cached;
        }

        // Cache miss — generate the prompt (reuse the already-computed table set).
        let prompt = self.build_system_prompt_for_tables(&relevant_tables);

        // Insert into cache.
        cache.insert(key, prompt.clone()).await;

        prompt
    }

    /// Selects relevant tables using direct name matches and lightweight TF-IDF scoring.
    ///
    /// A direct match also includes one-hop foreign-key neighbors. If no table scores,
    /// all tables are returned so that retrieval cannot accidentally hide the answer.
    pub fn filter_relevant_tables(&self, user_question: &str) -> Vec<String> {
        if self.schema.tables.is_empty() {
            return Vec::new();
        }

        let question_lower = user_question.to_lowercase();
        let question_tokens: HashSet<String> =
            meaningful_tokens(user_question).into_iter().collect();
        if question_tokens.is_empty() {
            return self.all_table_names();
        }

        let documents = self
            .schema
            .tables
            .iter()
            .map(table_document_tokens)
            .collect::<Vec<_>>();
        let document_frequency = document_frequency(&documents);
        let document_count = documents.len() as f64;
        let mut scores = HashMap::new();

        for (table, document) in self.schema.tables.iter().zip(&documents) {
            let mut score = tf_idf_overlap(
                &question_tokens,
                document,
                &document_frequency,
                document_count,
            );

            if phrase_matches(&question_lower, &question_tokens, &table.name) {
                score += 100.0;
            }
            for column in &table.columns {
                if phrase_matches(&question_lower, &question_tokens, &column.name) {
                    score += if is_generic_column_name(&column.name) {
                        2.0
                    } else {
                        20.0
                    };
                }
            }

            if score > 0.0 {
                scores.insert(table.name.clone(), score);
            }
        }

        if scores.is_empty() {
            return self.all_table_names();
        }

        let matched = scores.keys().cloned().collect::<HashSet<_>>();
        let expanded = self.expand_foreign_key_neighbors(&matched);
        self.schema
            .tables
            .iter()
            .filter(|table| expanded.contains(&table.name))
            .map(|table| table.name.clone())
            .collect()
    }

    /// Returns the shared schema snapshot used by the builder.
    pub fn schema(&self) -> &Arc<SchemaSnapshot> {
        &self.schema
    }

    /// Returns the dialect constraints used by the builder.
    pub fn dialect(&self) -> &DialectProfile {
        &self.dialect
    }

    /// Returns the policy constraints used by the builder.
    pub fn policy(&self) -> &PolicyConfig {
        &self.policy
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
                .map(|c| format!("{} {}", c.name, data_type_name(c.data_type)))
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
    }

    fn push_dialect_constraints(&self, prompt: &mut String) {
        let dialect_name = sql_dialect_name(self.dialect.dialect);

        let feature_flags: Vec<String> = [
            ("CTE", self.dialect.supports_cte),
            ("Window", self.dialect.supports_window_functions),
            ("JSON", self.dialect.supports_json_operations),
            ("DISTINCT", self.dialect.allow_distinct),
            ("OFFSET", self.dialect.supports_offset),
            ("FETCH", self.dialect.supports_fetch),
        ]
        .iter()
        .map(|(name, enabled)| {
            if *enabled {
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

    fn push_output_schema(&self, prompt: &mut String) {
        let root_schema = schema_for!(QueryPlan);
        let json_schema = serde_json::to_value(root_schema)
            .map(|mut schema| {
                remove_schema_descriptions(&mut schema);
                schema
            })
            .and_then(|schema| serde_json::to_string(&schema))
            .unwrap_or_else(|error| {
                format!(
                    "{{\"schema_generation_error\":\"{}\"}}",
                    json_string_fragment(&error.to_string())
                )
            });
        prompt.push_str(
            "## Required JSON Output\n\
             Structure:\n\
             - select: [Projection] (type: column → {table?, column, alias?} | expr → {expression, alias?} | star → {table?})\n\
             - from: {table, alias?}\n\
             - where: optional Predicate (type: comparison/and/or/not/between/in/like/is_null/exists)\n\
             - joins: optional [{join_type, right_table: FromClause, on: Predicate}]\n\
             - group_by: optional [Expression] | having: optional Predicate\n\
             - order_by: optional [{expr: Expression, descending: bool}]\n\
             - limit, offset: optional integer | ctes: optional [{name, query: QueryPlan}]\n\
             \n\
             Use the tagged type variants. Return a data instance — not a schema definition.\n\
             Output JSON only: no fences, comments, or raw SQL.\n\
             \n\
             ```json\n",
        );
        prompt.push_str(&json_schema);
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
                "type": "column",
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
             A: {{\"select\":[{{\"type\":\"column\",\"table\":\"orders\",\"column\":\"id\",\"alias\":null}},{{\"type\":\"column\",\"table\":\"orders\",\"column\":\"total\",\"alias\":null}}],\"from\":{{\"table\":\"orders\",\"alias\":null}},\"where\":{{\"type\":\"comparison\",\"left\":{{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"}},\"op\":\"gt\",\"right\":{{\"type\":\"literal\",\"value\":150,\"data_type\":\"float\"}}}},\"order_by\":[{{\"expr\":{{\"type\":\"column_ref\",\"column\":\"total\",\"table\":\"orders\"}},\"descending\":true}}],\"limit\":10}}\n\
             \n\
             The real response must obey the current schema and dialect.\n",
        );
    }

    /// Pushes a compact reminder about the `type` tag field.
    ///
    /// The JSON Schema above already defines every allowed `type` value, so
    /// this section only adds a reminder and a short example to help models
    /// that do not strictly enforce the schema (e.g. local Ollama models).
    fn push_type_guidance(&self, prompt: &mut String) {
        prompt.push_str(
            "## JSON Type Reminder\n\
             Every tagged object must include a `\"type\"` field matching the JSON Schema above.\n\
             \n\
             Example of a nested `WHERE`:\n\
             ```json\n\
             {\"where\": {\"type\": \"and\",\n\
               \"left\": {\"type\": \"comparison\", \"left\": {\"type\": \"column_ref\", \"column\": \"total\", \"table\": \"orders\"}, \"op\": \"gt\", \"right\": {\"type\": \"literal\", \"value\": 150, \"data_type\": \"float\"}},\n\
               \"right\": {\"type\": \"comparison\", \"left\": {\"type\": \"column_ref\", \"column\": \"status\", \"table\": \"orders\"}, \"op\": \"eq\", \"right\": {\"type\": \"literal\", \"value\": \"completed\", \"data_type\": \"string\"}}}}\n\
             ```\n\
             \n",
        );
    }

    fn column_visible(
        &self,
        table: &TableSchema,
        column: &ColumnSchema,
        policy: Option<&TablePolicy>,
    ) -> bool {
        if self.policy.global_denied_columns.iter().any(|denied| {
            denied == &column.name || denied == &format!("{}.{}", table.name, column.name)
        }) {
            return false;
        }
        let Some(policy) = policy else {
            return true;
        };
        if !policy.allowed || policy.denied_columns.contains(&column.name) {
            return false;
        }
        match &policy.allowed_columns {
            Some(allowed) => allowed.contains(&column.name),
            None => true,
        }
    }

    fn expand_foreign_key_neighbors(&self, matched: &HashSet<String>) -> HashSet<String> {
        let mut expanded = matched.clone();
        for table_name in matched {
            // Forward: add FK targets of matched tables.
            if let Some(table) = self.schema.get_table(table_name) {
                for column in &table.columns {
                    if let Some(fk) = &column.foreign_key
                        && self.schema.get_table(&fk.foreign_table).is_some()
                    {
                        expanded.insert(fk.foreign_table.clone());
                    }
                }
            }
            // Reverse: add tables whose FK points to this matched table.
            if let Some(referencing_tables) = self.reverse_fk_index.get(table_name) {
                for ref_table in referencing_tables {
                    if self.schema.get_table(ref_table).is_some() {
                        expanded.insert(ref_table.clone());
                    }
                }
            }
        }
        expanded
    }

    fn all_table_names(&self) -> Vec<String> {
        self.schema
            .tables
            .iter()
            .map(|table| table.name.clone())
            .collect()
    }
}

fn table_document_tokens(table: &TableSchema) -> HashMap<String, usize> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for token in meaningful_tokens(&table.name) {
        *freq.entry(token).or_insert(0) += 1;
    }
    if let Some(description) = &table.description {
        for token in meaningful_tokens(description) {
            *freq.entry(token).or_insert(0) += 1;
        }
    }
    for column in &table.columns {
        for token in meaningful_tokens(&column.name) {
            *freq.entry(token).or_insert(0) += 1;
        }
        if let Some(description) = &column.description {
            for token in meaningful_tokens(description) {
                *freq.entry(token).or_insert(0) += 1;
            }
        }
    }
    freq
}

fn document_frequency(documents: &[HashMap<String, usize>]) -> HashMap<String, usize> {
    let mut frequency = HashMap::new();
    for document in documents {
        for token in document.keys() {
            *frequency.entry(token.clone()).or_insert(0) += 1;
        }
    }
    frequency
}

fn tf_idf_overlap(
    question_tokens: &HashSet<String>,
    document: &HashMap<String, usize>,
    document_frequency: &HashMap<String, usize>,
    document_count: f64,
) -> f64 {
    question_tokens
        .iter()
        .filter_map(|token| {
            let tf = document.get(token).copied()?;
            let df = document_frequency.get(token).copied().unwrap_or(0) as f64;
            let idf = ((document_count + 1.0) / (df + 1.0)).ln() + 1.0;
            Some(tf as f64 * idf)
        })
        .sum()
}

fn meaningful_tokens(text: &str) -> Vec<String> {
    tokenize(text)
        .into_iter()
        .filter(|token| !is_stop_word(token))
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            token.push(character);
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

fn phrase_matches(
    question_lower: &str,
    question_tokens: &HashSet<String>,
    candidate: &str,
) -> bool {
    let candidate_lower = candidate.to_lowercase();
    if candidate_lower.len() > 2 && question_lower.contains(&candidate_lower) {
        return true;
    }
    let candidate_tokens = meaningful_tokens(candidate);
    !candidate_tokens.is_empty()
        && candidate_tokens
            .iter()
            .all(|token| question_tokens.contains(token))
}

fn is_stop_word(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "all"
            | "by"
            | "for"
            | "from"
            | "get"
            | "give"
            | "how"
            | "in"
            | "list"
            | "many"
            | "of"
            | "on"
            | "please"
            | "show"
            | "the"
            | "to"
            | "what"
            | "which"
            | "with"
    )
}

fn is_generic_column_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "id" | "name" | "created_at" | "updated_at"
    )
}

fn optional_limit(limit: Option<usize>) -> String {
    limit.map_or_else(|| "no explicit limit".to_owned(), |limit| limit.to_string())
}

fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Int => "int",
        DataType::Float => "float",
        DataType::String => "string",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
        DataType::Timestamp => "timestamp",
        DataType::Json => "json",
        DataType::Null => "null",
        DataType::Uuid => "uuid",
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

fn remove_schema_descriptions(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.remove("description");
            for value in object.values_mut() {
                remove_schema_descriptions(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                remove_schema_descriptions(value);
            }
        }
        _ => {}
    }
}

fn json_string_fragment(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Builds a reverse foreign-key index: maps each `foreign_table` → list of
/// local tables whose columns have a foreign key pointing to it.
/// This is used by [`PromptBuilder::expand_foreign_key_neighbors`] to avoid
/// O(n²) traversal on every query.
fn build_reverse_fk_index(schema: &SchemaSnapshot) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for table in &schema.tables {
        for column in &table.columns {
            if let Some(fk) = &column.foreign_key {
                index
                    .entry(fk.foreign_table.clone())
                    .or_default()
                    .push(table.name.clone());
            }
        }
    }
    index
}
