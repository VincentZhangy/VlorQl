use std::collections::{HashMap, HashSet};

use crate::schema::TableSchema;

use super::PromptBuilder;

impl PromptBuilder {
    /// Selects relevant tables for a user question.
    pub async fn filter_relevant_tables(&self, user_question: &str) -> Vec<String> {
        if self.schema.tables.is_empty() {
            return Vec::new();
        }

        #[cfg(feature = "vector-search")]
        if self.vector_search {
            let indexer = self.ensure_indexer();
            if let Some(ref indexer) = indexer {
                match indexer.search(user_question, 5).await {
                    Ok(tables) if !tables.is_empty() => {
                        let set: HashSet<String> = tables.into_iter().collect();
                        let expanded = self.expand_foreign_key_neighbors(&set);
                        let result: Vec<String> = self
                            .schema
                            .tables
                            .iter()
                            .filter(|t| expanded.contains(&t.name))
                            .map(|t| t.name.clone())
                            .collect();
                        if !result.is_empty() {
                            return result;
                        }
                    }
                    _ => { /* fall through to TF-IDF */ }
                }
            }
        }

        self.filter_relevant_tables_tfidf(user_question)
    }

    fn filter_relevant_tables_tfidf(&self, user_question: &str) -> Vec<String> {
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
