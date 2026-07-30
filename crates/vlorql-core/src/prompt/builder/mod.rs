//! Compact system prompt construction with DDL schema and minimal dialect constraints.

use crate::cache::hash_policy;
use crate::policy::{PolicyConfig, TablePolicy};
use crate::prompt::PromptSkill;
use crate::schema::{
    ColumnSchema, DialectProfile, SchemaSnapshot, TableSchema,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) mod filter;
pub(crate) mod prompt;

/// Builds strict LLM instructions from a shared schema, dialect, and policy.
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    pub(crate) schema: Arc<SchemaSnapshot>,
    pub(crate) dialect: DialectProfile,
    pub(crate) policy: PolicyConfig,
    pub(crate) policy_hash: u64,
    pub(crate) include_examples: bool,
    pub(crate) skill: Option<PromptSkill>,
    pub(crate) reverse_fk_index: HashMap<String, Vec<String>>,
    pub(crate) vector_search: bool,
    #[cfg(feature = "vector-search")]
    pub(crate) schema_indexer: Option<Arc<crate::prompt::schema_index::SchemaIndexer>>,
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
            skill: None,
            reverse_fk_index,
            vector_search: false,
            #[cfg(feature = "vector-search")]
            schema_indexer: None,
        }
    }

    /// Attach a prompt skill to inject custom instructions and examples.
    #[must_use]
    pub fn with_skill(mut self, skill: PromptSkill) -> Self {
        self.skill = Some(skill);
        self
    }

    /// Enables or disables the optional example section.
    #[must_use]
    pub fn with_examples(mut self, include_examples: bool) -> Self {
        self.include_examples = include_examples;
        self
    }

    /// Enables or disables vector-based schema retrieval via Qdrant.
    #[must_use]
    pub fn with_vector_search(mut self, enabled: bool) -> Self {
        self.vector_search = enabled;
        self
    }

    /// Attaches a pre-built [`SchemaIndexer`] for semantic table/column search.
    #[must_use]
    #[cfg(feature = "vector-search")]
    pub fn with_schema_indexer(
        mut self,
        indexer: Arc<crate::prompt::schema_index::SchemaIndexer>,
    ) -> Self {
        self.schema_indexer = Some(indexer);
        self
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

    /// Returns the configured [`SchemaIndexer`], if any.
    #[cfg(feature = "vector-search")]
    pub(crate) fn ensure_indexer(&self) -> Option<Arc<crate::prompt::schema_index::SchemaIndexer>> {
        if self.vector_search {
            self.schema_indexer.clone()
        } else {
            None
        }
    }

    /// Returns true if a feature should be forbidden by the active skill.
    pub(crate) fn is_forbidden_by_skill(&self, feature: &str) -> bool {
        self.skill
            .as_ref()
            .is_some_and(|s| s.forbid_features.iter().any(|f| f == feature))
    }

    pub(crate) fn column_visible(
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

    pub(crate) fn expand_foreign_key_neighbors(&self, matched: &HashSet<String>) -> HashSet<String> {
        let mut expanded = matched.clone();
        for table_name in matched {
            if let Some(table) = self.schema.get_table(table_name) {
                for column in &table.columns {
                    if let Some(fk) = &column.foreign_key
                        && self.schema.get_table(&fk.foreign_table).is_some()
                    {
                        expanded.insert(fk.foreign_table.clone());
                    }
                }
            }
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

    pub(crate) fn all_table_names(&self) -> Vec<String> {
        self.schema
            .tables
            .iter()
            .map(|table| table.name.clone())
            .collect()
    }
}

/// Builds a reverse foreign-key index: maps each `foreign_table` → list of
/// local tables whose columns have a foreign key pointing to it.
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
