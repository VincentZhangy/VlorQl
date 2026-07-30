use crate::{MAX_RETRY_FEEDBACK_ERRORS, StreamEvent};
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use vlorql_core::compile::SqlCompiler;
use vlorql_core::errors::{
    LlmErrorKind, SchemaErrorKind, ValidationErrorKind, ValidationErrors, VlorQLError,
};
use vlorql_core::policy::{PolicyConfig, PolicyEngine};
use vlorql_core::schema::{ArcSchemaSnapshot, DialectProfile, QueryPlan};
use vlorql_core::validate::ValidationPipeline;
use vlorql_llm::{LlmClient, detect_template_leak, parse_query_plan};

/// Sampling temperature for retry `attempt` (0 = first call). The first
/// call keeps the configured default (deterministic); each retry nudges
/// the temperature up so the model can escape a repeated bad output.
pub(crate) fn retry_temperature(base: f32, attempt: usize) -> Option<f32> {
    if attempt == 0 {
        None
    } else {
        Some((base + 0.2 * attempt as f32).min(1.0))
    }
}

pub(crate) fn format_retry_question_str(
    question: &str,
    error: &VlorQLError,
    attempt: usize,
) -> String {
    let raw = error.to_string();
    let feedback = if attempt == 0 {
        raw.lines().next().unwrap_or(&raw).to_owned()
    } else {
        raw
    };
    let hint = build_hint(error);
    format!(
        "{question}\n\n*** FEEDBACK ***\nThe previous query plan failed. Fix the specific errors below and return ONLY the corrected JSON QueryPlan.\nError: {feedback}{hint}"
    )
}

/// Build a specific, actionable hint based on the error type.
fn build_hint(error: &VlorQLError) -> String {
    match error {
        VlorQLError::Llm {
            kind: LlmErrorKind::ParseError { .. },
            ..
        } => {
            "\nTIP: The JSON structure was invalid. Check that all objects have a valid `type` field, all brackets are balanced, and no raw SQL is embedded in column names.".to_owned()
        }
        VlorQLError::Schema {
            kind: SchemaErrorKind::ColumnNotFound { table, column },
            ..
        } => {
            let available = error
                .details()
                .get("available_columns")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join("`, `")
                })
                .unwrap_or_default();
            if available.is_empty() {
                format!("\nTIP: Column `{column}` does not exist on table `{table}`. Use ONLY exact column names listed in the Schema section above.")
            } else {
                format!("\nTIP: Column `{column}` does not exist on table `{table}`. The ONLY valid columns in `{table}` are: `{available}`. Replace `{column}` with one of these.")
            }
        }
        VlorQLError::Validation {
            kind: ValidationErrorKind::MultipleErrors { .. },
            ..
        } => {
            let mut hints = Vec::new();
            if let Some(errors) = error.details().get("errors").and_then(|v| v.as_array()) {
                for e in errors {
                    // ColumnNotFound inside MultipleErrors
                    if let Some(col) = e.get("column").and_then(|v| v.as_str())
                        && let Some(table) = e.get("table").and_then(|v| v.as_str())
                    {
                        let available = e.get("available_columns")
                            .and_then(|v| v.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("`, `"))
                            .unwrap_or_default();
                        if !available.is_empty() {
                            hints.push(format!("Column `{table}.{col}` does NOT exist. Available: `{available}`"));
                        }
                    }
                    // SELECT * with GROUP BY
                    if let Some(msg) = e.get("message").and_then(|v| v.as_str()) {
                        if msg.contains("SELECT * with GROUP BY") || msg.contains("not allowed in GROUP BY") || msg.contains("not allowed in GROUP BY") {
                            hints.push("SELECT * and GROUP BY are incompatible. Replace star with explicit column_ref for each column.".to_owned());
                        }
                        if msg.contains("aggregate function") && msg.contains("not allowed in GROUP BY") {
                            hints.push("Aggregate functions (like `sum`, `count`, `string_agg`) cannot appear in GROUP BY. Remove them from group_by[].".to_owned());
                        }
                    }
                }
            }
            if hints.is_empty() {
                String::new()
            } else {
                format!("\n{}", hints.join("\n"))
            }
        }
        _ => String::new(),
    }
}

pub(crate) fn format_retry_question(
    original_question: &str,
    errors: &ValidationErrors,
    attempt: usize,
) -> String {
    // Tiered detail: the first retry gets a terse summary, later retries surface
    // more, capped by MAX_RETRY_FEEDBACK_ERRORS.
    let tier_cap = (1 + attempt).min(MAX_RETRY_FEEDBACK_ERRORS);
    let all = errors.as_slice();
    let shown = all.len().min(tier_cap);
    let feedback = all[..shown]
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let hints: Vec<String> = all[..shown]
        .iter()
        .filter_map(|error| match error {
            VlorQLError::Schema {
                kind: SchemaErrorKind::ColumnNotFound { table, column },
                ..
            } => {
                let available = error
                    .details()
                    .get("available_columns")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if available.is_empty() {
                    Some(format!("TIP: Column `{column}` does not exist on table `{table}`. Use exact column names from the Schema."))
                } else {
                    Some(format!("TIP: Column `{column}` does not exist on table `{table}`. Available: `{available}`."))
                }
            }
            _ => None,
        })
        .collect();
    let hints_str = if hints.is_empty() {
        String::new()
    } else {
        format!("\n{}", hints.join("\n"))
    };
    let omitted = all.len().saturating_sub(shown);
    let omitted_str = if omitted > 0 {
        format!("\n(… and {omitted} more errors omitted)")
    } else {
        String::new()
    };
    format!(
        "{original_question}\n\n*** FEEDBACK ***\nThe previous query plan failed. Fix the specific errors below and return ONLY the corrected JSON QueryPlan.\nErrors:\n{feedback}{hints_str}{omitted_str}"
    )
}

pub(crate) fn validation_errors_to_error(errors: ValidationErrors) -> VlorQLError {
    let error_list = errors.into_inner();
    if let [error] = error_list.as_slice() {
        return error.clone();
    }

    let count = error_list.len();
    VlorQLError::validation(
        ValidationErrorKind::MultipleErrors { count },
        json!({"errors": error_list}),
    )
}

#[expect(clippy::too_many_arguments)]
pub(crate) async fn run_stream_with_retry(
    event_tx: mpsc::UnboundedSender<Result<StreamEvent, VlorQLError>>,
    llm_client: Arc<dyn LlmClient>,
    mut question: String,
    system_prompt: String,
    schema: ArcSchemaSnapshot,
    dialect: DialectProfile,
    policy: PolicyConfig,
    compiler: Arc<dyn SqlCompiler>,
    max_retries: usize,
) {
    for attempt in 0..=max_retries {
        let stream_result = match llm_client
            .stream_plan(question.clone(), system_prompt.clone())
            .await
        {
            Ok(sr) => sr,
            Err(error) => {
                if error.is_retryable() && attempt < max_retries {
                    question = format_retry_question_str(&question, &error, attempt);
                    continue;
                }
                let _ = event_tx.send(Err(error));
                return;
            }
        };

        let mut buffer = String::new();
        let mut stream = stream_result.stream;
        let mut stream_ok = true;
        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    buffer.push_str(&chunk);
                    if event_tx.send(Ok(StreamEvent::TextChunk(chunk))).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if error.is_retryable() && attempt < max_retries {
                        question = format_retry_question_str(&question, &error, attempt);
                        stream_ok = false;
                        break;
                    }
                    let _ = event_tx.send(Err(error));
                    return;
                }
            }
        }
        if !stream_ok {
            continue;
        }

        let event = process_assembled_text(
            buffer,
            Arc::clone(&schema),
            dialect.clone(),
            policy.clone(),
            Arc::clone(&compiler),
        );
        match event {
            StreamEvent::Error(ref error) if error.is_retryable() && attempt < max_retries => {
                question = format_retry_question_str(&question, error, attempt);
                continue;
            }
            _ => {
                let _ = event_tx.send(Ok(event));
                if let Some(usage) = *stream_result.usage.lock().await {
                    let _ = event_tx.send(Ok(StreamEvent::TokenUsage(usage)));
                }
                return;
            }
        }
    }
}

pub(crate) fn process_assembled_text(
    buffer: String,
    schema: ArcSchemaSnapshot,
    dialect: DialectProfile,
    policy: PolicyConfig,
    compiler: Arc<dyn SqlCompiler>,
) -> StreamEvent {
    if let Some(details) = detect_template_leak(&buffer) {
        return StreamEvent::Error(VlorQLError::llm(
            LlmErrorKind::ParseError { details },
            json!({
                "source": "stream_assistant_content",
                "buffer_length": buffer.len(),
            }),
        ));
    }
    let plan: QueryPlan = match parse_query_plan(&buffer, None) {
        Ok(plan) => plan,
        Err(error) => {
            return StreamEvent::Error(VlorQLError::llm(
                LlmErrorKind::ParseError {
                    details: format!("assistant content is not a valid QueryPlan: {error}"),
                },
                json!({
                    "source": "stream_assistant_content",
                    "buffer_length": buffer.len(),
                }),
            ));
        }
    };
    let validation =
        ValidationPipeline::new(Arc::clone(&schema), dialect, PolicyEngine::new(policy))
            .validate_repairing(&plan);
    match validation {
        Ok(validated) => match compiler.compile(&validated) {
            Ok(_) => StreamEvent::PlanComplete(Box::new(plan)),
            Err(error) => StreamEvent::Error(error),
        },
        Err(errors) => StreamEvent::Error(validation_errors_to_error(errors)),
    }
}
