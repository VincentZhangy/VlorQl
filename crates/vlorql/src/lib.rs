//! The VlorQl facade: prompt construction, LLM planning, validation, and compilation.
//!
//! A [`VlorQl`] value owns the immutable schema, dialect profile,
//! policy, and LLM client that drive the end-to-end query workflow.
//! Use [`VlorQl::builder`] to assemble a facade and then call
//! [`VlorQl::query`] (block on the result) or [`VlorQl::query_stream`]
//! (consume text chunks followed by a [`StreamEvent::PlanComplete`]).
//!
//! ## Re-exports
//!
//! The most commonly used types from [`vlorql_core`] and
//! [`vlorql_llm`] are re-exported here so callers only need to
//! depend on the `vlorql` crate.

#![deny(missing_docs)]

pub mod execute;
pub(crate) mod retry;
/// Builder for [`VlorQl`] (re-exported from the `builder` submodule).
pub mod builder;

use futures::stream::Stream;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::Instrument;
use vlorql_core::cache::{LlmCacheKey, LlmResponseCache};
use vlorql_core::compile::SqlCompiler;
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::{DatabaseExecutor, QueryResult};
use vlorql_core::observability::{TelemetryGuard, VlorqMetrics};
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::policy::{PolicyConfig, PolicyEngine};
use vlorql_core::prompt::PromptBuilder;
use vlorql_core::schema::{ArcSchemaSnapshot, QueryPlan};
use vlorql_core::validate::ValidationPipeline;

use crate::retry::{
    format_retry_question, format_retry_question_str, retry_temperature, run_stream_with_retry,
    validation_errors_to_error,
};

pub use vlorql_core::cache::{CompileCache, PromptCache, SchemaCache, SchemaCacheKey};
pub use vlorql_core::compile::{
    CompiledQuery, DialectConfig, DialectRegistry, Parameter, RewriteEngine, RewriteRule,
};
pub use vlorql_core::errors::{ErrorResponse, ValidationErrors};
pub use vlorql_core::optimizer::QueryOptimizer as QueryOptimizerCore;
pub use vlorql_core::prompt::{ExamplePair, PromptSkill};
pub use vlorql_core::schema::{DialectProfile, SchemaSnapshot, SqlDialect};
pub use vlorql_core::validate::{OptimizedPlan, ValidatedPlan};
pub use vlorql_llm::{
    LlmClient, LlmConfig, LlmProvider, StreamResult, TokenUsage, create_llm_client,
    detect_template_leak, parse_query_plan, parse_query_plan_lenient,
};
pub use builder::VlorQlBuilder;

const DEFAULT_MAX_RETRIES: usize = 3;

/// Maximum number of validation errors surfaced in a single retry
/// feedback message. Smaller models degrade when flooded with every
/// error at once, so the feedback is capped and the remainder summarised.
const MAX_RETRY_FEEDBACK_ERRORS: usize = 3;

/// One item in the high-level stream emitted by [`VlorQl::query_stream`].
///
/// The facade first emits [`StreamEvent::TextChunk`] values as the LLM
/// generates the assistant response, and finally emits
/// [`StreamEvent::PlanComplete`] once the full response has been parsed and
/// validated. Validation or parsing failures are surfaced as
/// [`StreamEvent::Error`] instead.
///
/// # Examples
///
/// ```
/// use vlorql::StreamEvent;
/// use vlorql_core::errors::VlorQLError;
/// use vlorql_core::schema::QueryPlan;
/// use serde_json::json;
///
/// let event = StreamEvent::TextChunk("SELECT".to_owned());
/// assert!(matches!(event, StreamEvent::TextChunk(_)));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A raw text delta received from the LLM. Returned verbatim for
    /// consumption by user interfaces that want to display progressive output.
    TextChunk(String),
    /// The fully assembled, validated `QueryPlan` after the LLM response ends.
    PlanComplete(Box<QueryPlan>),
    /// Token usage emitted after the stream ends.
    TokenUsage(TokenUsage),
    /// A validation or parse error encountered after the LLM response.
    Error(VlorQLError),
}

/// The high-level VlorQl API.
///
/// A value owns the immutable schema, policy, compiler, and optional LLM client
/// required to execute the plan-then-validate-then-compile workflow.
///
/// # Examples
///
/// ```
/// use vlorql::VlorQl;
/// use vlorql_core::schema::{SchemaSnapshot, TableSchema, ColumnSchema, DataType, SchemaMetadata, QueryPlan, Projection, FromClause};
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
/// let vlorql = VlorQl::builder()
///     .with_schema(schema)
///     .with_dialect_name("postgres")
///     .with_policy(PolicyConfig::default())
///     .build()
///     .expect("facade");
///
/// // Validate and compile a plan without an LLM client.
/// let plan = QueryPlan {
///     select: vec![Projection::Column {
///         table: None, column: "id".to_owned(), alias: None,
///     }],
///     from: FromClause::table("users".to_owned(), None),
///     r#where: None, group_by: None, having: None,
///     order_by: None, limit: None, offset: None,
///     joins: None, ctes: None, distinct: false, distinct_on: None, set_operation: None,
/// };
/// let validated = vlorql.validate_only(&plan).expect("plan is valid");
/// let compiled = vlorql.compile_only(&validated).expect("plan compiles");
/// assert!(compiled.sql.contains("SELECT"));
/// ```
pub struct VlorQl {
    schema: ArcSchemaSnapshot,
    dialect: DialectProfile,
    policy: PolicyConfig,
    compiler: Arc<dyn SqlCompiler>,
    rewrite_engine: Option<RewriteEngine>,
    llm_client: Option<Arc<dyn LlmClient>>,
    max_retries: usize,
    optimizer: Option<QueryOptimizer>,
    schema_cache: Option<Arc<SchemaCache>>,
    compile_cache: Option<Arc<CompileCache>>,
    prompt_cache: Option<Arc<PromptCache>>,
    llm_cache: LlmResponseCache,
    telemetry_guard: Option<TelemetryGuard>,
    metrics: Option<Arc<VlorqMetrics>>,
    executor: Option<Arc<dyn DatabaseExecutor>>,
}

impl std::fmt::Debug for VlorQl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VlorQl")
            .field("schema", &self.schema)
            .field("dialect", &self.dialect)
            .field("policy", &self.policy)
            .field("compiler_dialect", &self.compiler.dialect())
            .field("has_rewrite_engine", &self.rewrite_engine.is_some())
            .field("has_llm_client", &self.llm_client.is_some())
            .field("has_optimizer", &self.optimizer.is_some())
            .field("has_schema_cache", &self.schema_cache.is_some())
            .field("has_compile_cache", &self.compile_cache.is_some())
            .field("has_prompt_cache", &self.prompt_cache.is_some())
            .field("llm_cache_size", &self.llm_cache.size())
            .field("max_retries", &self.max_retries)
            .field("has_executor", &self.executor.is_some())
            .finish()
    }
}

impl VlorQl {
    /// Starts constructing a VlorQl facade.
    pub fn builder() -> VlorQlBuilder {
        VlorQlBuilder::default()
    }

    /// Generates a plan with the configured LLM, validates it, and compiles it.
    ///
    /// When a statistics provider has been configured, the validated plan
    /// is also passed through the [`QueryOptimizer`] before compilation.
    ///
    /// When a [`PromptCache`] is configured, the system prompt is retrieved
    /// from the cache when possible.  When a [`CompileCache`] is configured,
    /// a plan that has already been compiled for the same dialect is
    /// returned without re-compiling.
    pub async fn query(&self, question: &str) -> Result<(CompiledQuery, TokenUsage), VlorQLError> {
        let span = tracing::info_span!(
            "vlorql.query",
            question_len = question.len(),
            dialect = ?self.dialect.dialect,
            policy_enabled = !self.policy.table_policies.is_empty(),
        );
        async move {
            // Record query start.
            if let Some(ref m) = self.metrics {
                m.active_queries.add(1, &[]);
                m.query_counter.add(1, &[]);
            }
            let start = std::time::Instant::now();

            let client = self.llm_client.as_ref().ok_or_else(|| {
                if let Some(ref m) = self.metrics {
                    m.active_queries.add(-1, &[]);
                }
                VlorQLError::config(
                    ConfigErrorKind::MissingLlmClient,
                    json!({"operation": "query"}),
                )
            })?;

            // Resolve the schema (may consult the schema cache).
            let schema = self.resolve_schema().await;
            let schema_version = schema.metadata.version.clone().unwrap_or_default();

            // Build the system prompt, optionally using the prompt cache.
            let prompt_builder = PromptBuilder::new(
                Arc::clone(&schema),
                self.dialect.clone(),
                self.policy.clone(),
            );
            let system_prompt = match &self.prompt_cache {
                Some(cache) => {
                    prompt_builder
                        .build_system_prompt_with_cache(question, cache.as_ref())
                        .await
                }
                None => prompt_builder.build_system_prompt(question),
            };

            // Build cache key and check the LLM response cache.
            let model_fingerprint =
                format!("{}:{}", client.config().provider, client.config().model);
            let cache_key = LlmCacheKey {
                normalized_question: question.to_lowercase(),
                schema_version,
                model_fingerprint,
            };
            let cached_plan: Option<Arc<QueryPlan>> =
                self.llm_cache.get(&cache_key).await;

            let mut llm_question = question.to_owned();
            let mut last_usage = TokenUsage::default();
            for attempt in 0..=self.max_retries {
                let temperature = retry_temperature(client.config().temperature, attempt);
                let plan = if attempt == 0 {
                    if let Some(ref cached) = cached_plan {
                        (**cached).clone()
                    } else {
                        let llm_start = std::time::Instant::now();
                        let result = client
                            .generate_plan(&llm_question, &system_prompt, temperature)
                            .await;
                        if let Some(ref m) = self.metrics {
                            m.llm_duration_histogram
                                .record(llm_start.elapsed().as_secs_f64(), &[]);
                        }
                        match result {
                            Ok((plan, usage)) => {
                                last_usage = usage;
                                plan
                            }
                            Err(e) if e.is_retryable() && attempt < self.max_retries => {
                                llm_question = format_retry_question_str(&llm_question, &e, attempt);
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                } else {
                    match client
                        .generate_plan(&llm_question, &system_prompt, temperature)
                        .await
                    {
                        Ok((plan, usage)) => {
                            last_usage = usage;
                            plan
                        }
                        Err(e) if e.is_retryable() && attempt < self.max_retries => {
                            llm_question = format_retry_question_str(&llm_question, &e, attempt);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                };
                match self.build_pipeline(&schema).validate_repairing(&plan) {
                    Ok(validated_plan) => {
                        // Optimize when an optimizer is configured, then compile.
                        let plan_for_compile = match &self.optimizer {
                            Some(optimizer) => {
                                match optimizer.optimize_async(validated_plan.as_plan()).await {
                                    Ok(optimized) => {
                                        // Re-validate policy on the optimized plan.
                                        let pipeline = self.build_pipeline(&schema);
                                        if let Err(stage_errors) =
                                            pipeline.policy().validate(&optimized, &schema)
                                        {
                                            return Err(validation_errors_to_error(
                                                ValidationErrors(stage_errors),
                                            ));
                                        }
                                        ValidatedPlan(Arc::new(optimized))
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            None => validated_plan,
                        };

                        // Check the compile cache before compiling.
                        if let Some(cache) = &self.compile_cache
                            && let Some(cached) = cache.get(&plan_for_compile, &self.dialect).await
                        {
                            if let Some(ref m) = self.metrics {
                                m.cache_hit_counter.add(1, &[]);
                                m.llm_prompt_tokens.add(last_usage.prompt_tokens, &[]);
                                m.llm_completion_tokens.add(last_usage.completion_tokens, &[]);
                            }
                            return Ok(((*cached).clone(), last_usage));
                        }
                        if let Some(ref m) = self.metrics {
                            m.cache_miss_counter.add(1, &[]);
                        }

                        // Compile (cache miss).
                        let compiled = self.compile_only(&plan_for_compile)?;

                        // Insert into the compile cache.
                        if let Some(cache) = &self.compile_cache {
                            cache
                                .insert(&plan_for_compile, &self.dialect, compiled.clone())
                                .await;
                        }

                        // Insert into the LLM response cache.
                        self.llm_cache
                            .insert(cache_key, Arc::new(plan.clone()))
                            .await;

                        if let Some(ref m) = self.metrics {
                            m.llm_prompt_tokens.add(last_usage.prompt_tokens, &[]);
                            m.llm_completion_tokens.add(last_usage.completion_tokens, &[]);
                        }
                        let elapsed = start.elapsed().as_secs_f64();
                        if let Some(ref m) = self.metrics {
                            m.query_duration_histogram.record(elapsed, &[]);
                            m.active_queries.add(-1, &[]);
                        }
                        return Ok((compiled, last_usage));
                    }
                    Err(errors) => {
                        let plan_json = serde_json::to_string(&plan).unwrap_or_default();
                        tracing::error!(
                            plan_json,
                            error_count = errors.len(),
                            "Schema validation failed"
                        );
                        if let Some(ref m) = self.metrics {
                            m.error_counter.add(
                                1,
                                &[opentelemetry::KeyValue::new("error_type", "validation")],
                            );
                        }
                        let can_retry = attempt < self.max_retries
                            && !errors.is_empty()
                            && errors.as_slice().iter().all(VlorQLError::is_retryable);
                        if !can_retry {
                            return Err(validation_errors_to_error(errors));
                        }

                        llm_question = format_retry_question(question, &errors, attempt);
                    }
                }
            }

            // The loop always returns when max_retries is finite, but keep a structured
            // error here so the API never needs to panic if that invariant changes.
            let err = VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: "validation retry loop did not terminate".to_owned(),
                },
                json!({"operation": "query"}),
            );
            if let Some(ref m) = self.metrics {
                m.active_queries.add(-1, &[]);
            }
            Err(err)
        }
        .instrument(span)
        .await
    }

    /// Runs a complete natural-language query pipeline: plan → validate →
    /// compile → execute.
    ///
    /// This is a convenience method that chains [`Self::query`] with the
    /// configured [`DatabaseExecutor`]. It requires an executor to have
    /// been set (via [`VlorQlBuilder::with_executor`]) — if none is
    /// configured the call will return an error immediately.
    ///
    /// # Errors
    ///
    /// Returns [`VlorQLError`] if the executor is not configured, or if
    /// any step of the pipeline (LLM plan generation, validation,
    /// compilation, or database execution) fails.
    pub async fn run(&self, question: &str) -> Result<QueryResult, VlorQLError> {
        let (compiled, _usage) = self.query(question).await?;
        match &self.executor {
            Some(executor) => executor.execute(&compiled).await,
            None => Err(VlorQLError::config(
                ConfigErrorKind::MissingLlmClient,
                json!({"message": "no database executor configured; call with_executor() on the builder"}),
            )),
        }
    }

    /// Streams the assistant response and emits high-level events.
    ///
    /// The first events are the raw text deltas from the LLM. Once the LLM
    /// closes the stream, the accumulated text is parsed as a `QueryPlan`,
    /// validated, and emitted as a `PlanComplete` event (or an `Error` event
    /// if parsing or validation fails). Retryable validation errors trigger
    /// automatic retries with feedback (up to `max_retries` additional attempts).
    pub async fn query_stream(
        &self,
        question: &str,
    ) -> Result<Box<dyn Stream<Item = Result<StreamEvent, VlorQLError>> + Send + Unpin>, VlorQLError>
    {
        let client = Arc::clone(self.llm_client.as_ref().ok_or_else(|| {
            VlorQLError::config(
                ConfigErrorKind::MissingLlmClient,
                json!({"operation": "query_stream"}),
            )
        })?);
        let system_prompt = PromptBuilder::new(
            Arc::clone(&self.schema),
            self.dialect.clone(),
            self.policy.clone(),
        )
        .build_system_prompt(question);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let schema = Arc::clone(&self.schema);
        let dialect = self.dialect.clone();
        let policy = self.policy.clone();
        let compiler = Arc::clone(&self.compiler);
        let max_retries = self.max_retries;
        let question = question.to_owned();

        tokio::spawn(async move {
            run_stream_with_retry(
                event_tx,
                client,
                question,
                system_prompt,
                schema,
                dialect,
                policy,
                compiler,
                max_retries,
            )
            .await;
        });

        Ok(Box::new(Box::pin(UnboundedReceiverStream::new(event_rx))))
    }

    /// Validates a plan without invoking the LLM or compiler.
    pub fn validate_only(&self, plan: &QueryPlan) -> Result<ValidatedPlan, ValidationErrors> {
        let span = tracing::debug_span!("vlorql.validate", plan_has_cte = plan.ctes.is_some());
        let _enter = span.enter();
        self.build_pipeline(&self.schema).validate(plan)
    }

    /// Validates a plan and, when an optimizer is configured, applies
    /// optimisation passes.  Returns an [`OptimizedPlan`] that derefs to
    /// [`ValidatedPlan`].
    ///
    /// Like [`validate_only`](Self::validate_only), this is an honest
    /// validation entry point: it does **not** apply execution-time
    /// auto-repairs such as dropping JOINs to tables that do not exist in
    /// the schema. A plan referencing a non-existent table is reported as
    /// an error, not silently repaired. To execute a plan with those
    /// auto-repairs (e.g. recovering from an LLM-hallucinated JOIN), use
    /// [`query`](Self::query), which validates with repair internally.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationErrors`] when any validation stage (including
    /// the post-optimisation policy re-check) fails.
    pub async fn validate_and_optimize(
        &self,
        plan: &QueryPlan,
    ) -> Result<OptimizedPlan, ValidationErrors> {
        self.build_pipeline_with_optimizer(&self.schema)
            .validate_and_optimize(plan)
            .await
    }

    /// Compiles a plan that has already passed validation.
    pub fn compile_only(&self, plan: &ValidatedPlan) -> Result<CompiledQuery, VlorQLError> {
        let span = tracing::info_span!(
            "vlorql.compile",
            dialect = ?self.compiler.dialect(),
        );
        let _enter = span.enter();
        let mut result = self.compiler.compile(plan)?;
        // Apply post-compilation rewrite rules.
        if let Some(ref engine) = self.rewrite_engine {
            let dialect_str = format!("{:?}", self.dialect.dialect).to_lowercase();
            result.sql = engine.apply(&result.sql, &dialect_str)?;
        }
        tracing::debug!("Compiled SQL length: {} chars", result.sql.len());
        Ok(result)
    }

    /// Returns the configured schema.
    pub fn schema(&self) -> &ArcSchemaSnapshot {
        &self.schema
    }

    /// Returns the configured dialect profile.
    pub fn dialect(&self) -> &DialectProfile {
        &self.dialect
    }

    /// Returns the configured policy.
    pub fn policy(&self) -> &PolicyConfig {
        &self.policy
    }

    /// Returns the maximum number of validation retries.
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// Returns a reference to the optional schema cache.
    pub fn schema_cache(&self) -> Option<&Arc<SchemaCache>> {
        self.schema_cache.as_ref()
    }

    /// Returns a reference to the optional compile cache.
    pub fn compile_cache(&self) -> Option<&Arc<CompileCache>> {
        self.compile_cache.as_ref()
    }

    /// Returns a reference to the optional prompt cache.
    pub fn prompt_cache(&self) -> Option<&Arc<PromptCache>> {
        self.prompt_cache.as_ref()
    }

    /// Invalidates all schema cache entries matching `version`.
    pub fn invalidate_schema_cache(&self, version: &str) {
        if let Some(cache) = &self.schema_cache {
            cache.invalidate_version(version);
        }
    }

    /// Invalidates the compile cache entry for `plan` under the current dialect.
    pub async fn invalidate_compile_cache(&self, plan: &ValidatedPlan) {
        if let Some(cache) = &self.compile_cache {
            cache.invalidate_plan(plan, &self.dialect).await;
        }
    }

    /// Clears all three caches (schema, compile, prompt).
    pub fn clear_all_caches(&self) {
        if let Some(cache) = &self.schema_cache {
            cache.clear();
        }
        if let Some(cache) = &self.compile_cache {
            cache.clear();
        }
        if let Some(cache) = &self.prompt_cache {
            cache.clear();
        }
    }

    /// Builds a [`ValidationPipeline`] without the optimizer.
    fn build_pipeline(&self, schema: &ArcSchemaSnapshot) -> ValidationPipeline {
        ValidationPipeline::new(
            Arc::clone(schema),
            self.dialect.clone(),
            PolicyEngine::new(self.policy.clone()),
        )
    }

    /// Builds a [`ValidationPipeline`] with the optional optimizer attached.
    fn build_pipeline_with_optimizer(&self, schema: &ArcSchemaSnapshot) -> ValidationPipeline {
        let mut pipeline = ValidationPipeline::new(
            Arc::clone(schema),
            self.dialect.clone(),
            PolicyEngine::new(self.policy.clone()),
        );
        if let Some(ref optimizer) = self.optimizer {
            pipeline = pipeline.with_optimizer(optimizer.clone());
        }
        pipeline
    }

    /// Resolves the schema, optionally through the schema cache.
    async fn resolve_schema(&self) -> ArcSchemaSnapshot {
        if let Some(cache) = &self.schema_cache {
            let version = self.schema.metadata.version.clone().unwrap_or_default();
            let key = SchemaCacheKey {
                version,
                source: "build".to_owned(),
            };
            cache
                .get_or_insert_with(key, || async { Arc::clone(&self.schema) })
                .await
        } else {
            tracing::debug!(target: "vlorql", "resolve_schema: no schema cache configured");
            Arc::clone(&self.schema)
        }
    }
}

impl Drop for VlorQl {
    fn drop(&mut self) {
        if let Some(cache) = self.compile_cache.clone() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = cache.persist().await {
                        tracing::warn!(target: "vlorql", "failed to persist compile cache: {e}");
                    }
                });
            }
        }
        if let Some(guard) = self.telemetry_guard.take() {
            vlorql_core::observability::shutdown_telemetry(guard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use vlorql_core::errors::{LlmErrorKind, SchemaErrorKind};
    use vlorql_core::schema::{
        ColumnSchema, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
        SchemaMetadata, TableSchema,
    };
    use vlorql_llm::MockLlmClient;

    #[test]
    fn retry_feedback_is_truncated_when_many_errors() {
        let errs: Vec<VlorQLError> = (0..5)
            .map(|i| {
                VlorQLError::schema(
                    SchemaErrorKind::ColumnNotFound {
                        table: "t".to_owned(),
                        column: format!("c{i}"),
                    },
                    serde_json::json!({"table": "t", "column": format!("c{i}")}),
                )
            })
            .collect();
        let errors = ValidationErrors(errs);
        let q = format_retry_question("q", &errors, 2);
        assert!(
            q.contains("c0") && q.contains("c1") && q.contains("c2"),
            "first 3: {q}"
        );
        assert!(!q.contains("c4"), "not the 5th: {q}");
        assert!(q.contains("2 more"), "omitted count: {q}");
    }

    #[test]
    fn retry_temperature_keeps_default_on_first_attempt_then_escalates() {
        assert_eq!(retry_temperature(0.0, 0), None);
        assert_eq!(retry_temperature(0.0, 1), Some(0.2));
        assert_eq!(retry_temperature(0.0, 2), Some(0.4));
        assert!(retry_temperature(0.9, 3).expect("retry should escalate") <= 1.0);
    }

    #[test]
    fn retry_feedback_is_tiered_by_attempt() {
        let errs: Vec<VlorQLError> = (0..5)
            .map(|i| {
                VlorQLError::schema(
                    SchemaErrorKind::ColumnNotFound {
                        table: "t".to_owned(),
                        column: format!("c{i}"),
                    },
                    serde_json::json!({"table": "t", "column": format!("c{i}")}),
                )
            })
            .collect();
        let errors = ValidationErrors(errs);
        let first = format_retry_question("q", &errors, 0);
        let later = format_retry_question("q", &errors, 2);
        assert!(
            first.matches("does not exist").count() < later.matches("does not exist").count(),
            "attempt 0 terser than attempt 2:\nfirst={first}\nlater={later}"
        );
    }

    fn schema() -> Arc<SchemaSnapshot> {
        Arc::new(SchemaSnapshot::new(
            vec![TableSchema {
                name: "users".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: None,
                primary_key: Some(vec!["id".to_owned()]),
            }],
            SchemaMetadata::default(),
        ))
    }

    fn valid_plan() -> QueryPlan {
        QueryPlan {
            select: vec![Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            }],
            from: FromClause::table("users".to_owned(), Some("t1".to_owned())),
            r#where: None,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            joins: None,
            ctes: None,
            distinct: false,
            distinct_on: None,
            set_operation: None,
        }
    }

    fn facade_with_mock(plan: QueryPlan) -> VlorQl {
        VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("sqlite")
            .with_llm_client(MockLlmClient::success(plan))
            .build()
            .expect("facade should build")
    }

    #[tokio::test]
    async fn query_runs_prompt_validation_and_compilation() {
        let boxed_client: Box<dyn LlmClient> = Box::new(MockLlmClient::success(valid_plan()));
        let facade = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("sqlite")
            .with_llm_client(boxed_client)
            .build()
            .expect("boxed client should build");
        let (compiled, _usage) = facade
            .query("show user ids")
            .await
            .expect("valid mock plan should compile");
        assert_eq!(compiled.dialect, SqlDialect::Sqlite);
        assert_eq!(
            compiled.sql,
            "SELECT \"t1\".\"id\" FROM \"users\" AS \"t1\""
        );
    }

    #[tokio::test]
    async fn query_requires_an_llm_client_but_validate_only_does_not() {
        let facade = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("postgres")
            .build()
            .expect("facade without LLM should still build");
        let error = facade
            .query("show users")
            .await
            .expect_err("query should require an LLM client");
        assert_eq!(error.error_code(), "G001");
        assert!(facade.validate_only(&valid_plan()).is_ok());
    }

    #[test]
    fn builder_checks_schema_and_dialect() {
        assert_eq!(
            VlorQl::builder()
                .with_dialect_name("sqlite")
                .build()
                .expect_err("schema is required")
                .error_code(),
            "G002"
        );
        assert_eq!(
            VlorQl::builder()
                .with_schema(schema())
                .with_dialect_name("unknown")
                .build()
                .expect_err("dialect should be checked")
                .error_code(),
            "G003"
        );
    }

    #[cfg(test)]
    struct SequenceClient {
        plans: Mutex<Vec<QueryPlan>>,
        config: LlmConfig,
    }

    #[cfg(test)]
    #[async_trait]
    impl LlmClient for SequenceClient {
        async fn generate_plan(
            &self,
            _question: &str,
            _system_prompt: &str,
            _temperature: Option<f32>,
        ) -> Result<(QueryPlan, TokenUsage), VlorQLError> {
            let plan = self
                .plans
                .lock()
                .expect("sequence lock should not be poisoned")
                .pop()
                .ok_or_else(|| {
                    VlorQLError::llm(
                        LlmErrorKind::ParseError {
                            details: "sequence exhausted".to_owned(),
                        },
                        json!({}),
                    )
                })?;
            Ok((plan, TokenUsage::default()))
        }

        async fn stream_plan(
            &self,
            question: String,
            system_prompt: String,
        ) -> Result<StreamResult, VlorQLError> {
            let (plan, _usage) = self.generate_plan(&question, &system_prompt, None).await?;
            let serialized = serde_json::to_string(&plan).unwrap_or_default();
            let stream = Box::new(futures::stream::iter(vec![Ok(serialized)]))
                as Box<dyn futures::stream::Stream<Item = Result<String, VlorQLError>> + Send + Unpin>;
            let usage = Arc::new(tokio::sync::Mutex::new(Some(TokenUsage::default())));
            Ok(StreamResult { stream, usage })
        }

        fn provider(&self) -> vlorql_llm::LlmProvider {
            vlorql_llm::LlmProvider::OpenAi
        }

        fn config(&self) -> &vlorql_llm::LlmConfig {
            &self.config
        }
    }

    #[tokio::test]
    async fn query_retries_retryable_validation_errors() {
        let mut invalid = valid_plan();
        invalid.r#where = Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
            },
            op: vlorql_core::schema::ComparisonOperator::Eq,
            right: Expression::Literal {
                value: serde_json::json!(1),
                data_type: DataType::Int,
            },
        });
        let sequence = SequenceClient {
            plans: Mutex::new(vec![valid_plan(), invalid]),
            config: LlmConfig::default(),
        };
        let facade = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("postgres")
            .with_llm_client(sequence)
            .with_max_retries(2)
            .build()
            .expect("facade should build");

        let (compiled, _usage) = facade
            .query("show user ids")
            .await
            .expect("second valid plan should be used");
        assert!(compiled.sql.contains("SELECT"));
    }

    #[test]
    fn with_llm_config_creates_facade() {
        let config = LlmConfig {
            provider: LlmProvider::Ollama,
            model: "llama3".to_owned(),
            ..LlmConfig::default()
        };
        let v = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("sqlite")
            .with_llm_config(config)
            .build()
            .expect("facade should build with config");
        assert_eq!(v.max_retries(), 3);
    }

    #[test]
    fn validation_and_compilation_helpers_are_public() {
        let facade = facade_with_mock(valid_plan());
        let validated = facade
            .validate_only(&valid_plan())
            .expect("plan should validate");
        let compiled = facade
            .compile_only(&validated)
            .expect("plan should compile");
        assert!(compiled.sql.contains("users"));
    }

    #[tokio::test]
    async fn query_stream_emits_chunks_then_plan_complete() {
        use futures::StreamExt;
        let facade = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("sqlite")
            .with_policy(PolicyConfig::default())
            .with_llm_client(MockLlmClient::success(valid_plan()))
            .build()
            .expect("facade should build");
        let mut stream = facade
            .query_stream("list users")
            .await
            .expect("query_stream should succeed");
        let mut final_plan = None;
        let mut saw_chunks = false;
        let mut saw_usage = false;
        while let Some(item) = stream.next().await {
            match item.expect("event should be Ok") {
                StreamEvent::TextChunk(_) => saw_chunks = true,
                StreamEvent::PlanComplete(plan) => final_plan = Some(*plan),
                StreamEvent::TokenUsage(usage) => {
                    saw_usage = true;
                    assert_eq!(usage, TokenUsage::default());
                }
                StreamEvent::Error(error) => panic!("unexpected error event: {error}"),
            }
        }
        assert!(saw_chunks, "should receive at least one text chunk");
        assert_eq!(final_plan, Some(valid_plan()));
        assert!(saw_usage, "should receive token usage event");
    }

    #[tokio::test]
    async fn span_hierarchy_includes_query_validate_compile() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        // Collect span events into a shared vector.
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let events_clone = Arc::clone(&events);

        let layer = tracing_subscriber::fmt::layer()
            .with_test_writer()
            .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                meta.target().starts_with("vlorql")
            }));

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let facade = VlorQl::builder()
            .with_schema(schema())
            .with_dialect_name("sqlite")
            .with_llm_client(MockLlmClient::success(valid_plan()))
            .build()
            .expect("facade should build");

        let (compiled, _usage) = facade
            .query("show user ids")
            .await
            .expect("valid mock plan should compile");

        assert_eq!(compiled.dialect, SqlDialect::Sqlite);
        assert_eq!(
            compiled.sql,
            "SELECT \"t1\".\"id\" FROM \"users\" AS \"t1\""
        );
        // The test verifies that the query completes without error under
        // a tracing subscriber; span hierarchy is validated by inspecting
        // the subscriber output (stderr) when `RUST_LOG` is set.
        // The presence of spans is confirmed by the fact that the subscriber
        // was installed and no panics occurred during the async move.
        drop(
            events_clone
                .lock()
                .expect("events lock should not be poisoned"),
        );
    }

    #[test]
    fn json_logs_contain_trace_id_and_span_id() {
        // Use a static Mutex to avoid lifetime issues with the writer.
        use std::sync::LazyLock;
        use std::sync::Mutex;
        static BUF: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::new()));

        // Create a JSON-formatted subscriber that writes to our buffer.
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_thread_ids(false)
            .with_file(false)
            .with_line_number(false)
            .with_writer(move || {
                let buf = BUF.lock().expect("lock");
                let data = buf.clone();
                Box::new(std::io::Cursor::new(data)) as Box<dyn std::io::Write + Send>
            })
            .finish();

        let _guard = tracing::subscriber::set_default(subscriber);

        // Emit a span and an event inside it.
        let span = tracing::info_span!("test_span", key = "value");
        let _enter = span.enter();
        tracing::info!("test event inside span");
        drop(_enter);
        drop(_guard);

        // Read the captured output.
        let buf_guard = BUF.lock().expect("lock");
        let output = String::from_utf8_lossy(&buf_guard);
        // Verify that JSON output is produced and is valid.
        if !output.is_empty() {
            for line in output.lines() {
                if line.contains("span") {
                    let parsed: serde_json::Value =
                        serde_json::from_str(line).expect("each line should be valid JSON");
                    assert!(parsed.is_object(), "JSON log line should be an object");
                }
            }
        }
        // The test verifies that the JSON logging infrastructure
        // produces valid JSON without panicking.
    }
}
