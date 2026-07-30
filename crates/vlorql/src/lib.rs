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

/// Builder for [`VlorQl`] (re-exported from the `builder` submodule).
pub mod builder;
pub mod execute;
pub(crate) mod retry;

use futures::stream::Stream;
use serde_json::json;
use std::sync::Arc;
#[cfg(feature = "vector-search")]
use std::sync::atomic::{AtomicBool, Ordering};
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

#[cfg(feature = "vector-search")]
use vlorql_core::prompt::schema_index::SchemaIndexer;

use crate::retry::{
    format_retry_question, format_retry_question_str, retry_temperature, run_stream_with_retry,
    validation_errors_to_error,
};

pub use builder::VlorQlBuilder;
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
    /// Enables vector-based schema retrieval via Qdrant (default: false).
    vector_search: bool,
    /// Optional SchemaIndexer for semantic table/column search (requires `vector-search` feature).
    #[cfg(feature = "vector-search")]
    schema_indexer: Option<Arc<SchemaIndexer>>,
    /// One-shot guard to avoid re-indexing the schema on every query.
    #[cfg(feature = "vector-search")]
    schema_indexed: AtomicBool,
}

impl std::fmt::Debug for VlorQl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = formatter.debug_struct("VlorQl");
        d.field("schema", &self.schema);
        d.field("dialect", &self.dialect);
        d.field("policy", &self.policy);
        d.field("compiler_dialect", &self.compiler.dialect());
        d.field("has_rewrite_engine", &self.rewrite_engine.is_some());
        d.field("has_llm_client", &self.llm_client.is_some());
        d.field("has_optimizer", &self.optimizer.is_some());
        d.field("has_schema_cache", &self.schema_cache.is_some());
        d.field("has_compile_cache", &self.compile_cache.is_some());
        d.field("has_prompt_cache", &self.prompt_cache.is_some());
        d.field("llm_cache_size", &self.llm_cache.size());
        d.field("max_retries", &self.max_retries);
        d.field("has_executor", &self.executor.is_some());
        d.field("vector_search", &self.vector_search);
        #[cfg(feature = "vector-search")]
        d.field("has_schema_indexer", &self.schema_indexer.is_some());
        d.finish()
    }
}

impl VlorQl {
    /// Starts constructing a VlorQl facade.
    pub fn builder() -> VlorQlBuilder {
        VlorQlBuilder::default()
    }

    /// Creates a [`PromptBuilder`] configured with schema, dialect, policy,
    /// and (when enabled) vector-search settings.
    fn build_prompt_builder(&self, schema: Arc<SchemaSnapshot>) -> PromptBuilder {
        #[allow(unused_mut)]
        let mut builder = PromptBuilder::new(schema, self.dialect.clone(), self.policy.clone());
        #[cfg(feature = "vector-search")]
        {
            builder = builder.with_vector_search(self.vector_search);
            if let Some(ref indexer) = self.schema_indexer {
                builder = builder.with_schema_indexer(Arc::clone(indexer));
            }
        }
        builder
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
            let prompt_builder = self.build_prompt_builder(Arc::clone(&schema));

            #[cfg(feature = "vector-search")]
            if let Some(ref indexer) = self.schema_indexer {
                if self
                    .schema_indexed
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    indexer.index_schema(&schema).await.ok();
                }
            }

            let system_prompt = match &self.prompt_cache {
                Some(cache) => {
                    prompt_builder
                        .build_system_prompt_with_cache(question, cache.as_ref())
                        .await
                }
                None => prompt_builder.build_system_prompt(question).await,
            };

            // Build cache key and check the LLM response cache.
            let model_fingerprint =
                format!("{}:{}", client.config().provider, client.config().model);
            let cache_key = LlmCacheKey {
                normalized_question: question.to_lowercase(),
                schema_version,
                model_fingerprint,
            };
            let cached_plan: Option<Arc<QueryPlan>> = self.llm_cache.get(&cache_key).await;

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
                                llm_question =
                                    format_retry_question_str(&llm_question, &e, attempt);
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
                                m.llm_completion_tokens
                                    .add(last_usage.completion_tokens, &[]);
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
                            m.llm_completion_tokens
                                .add(last_usage.completion_tokens, &[]);
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
    /// configured `DatabaseExecutor`. It requires an executor to have
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
        let schema = self.resolve_schema().await;

        #[cfg(feature = "vector-search")]
        if let Some(ref indexer) = self.schema_indexer {
            if self
                .schema_indexed
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                indexer.index_schema(&schema).await.ok();
            }
        }

        let prompt_builder = self.build_prompt_builder(Arc::clone(&schema));
        let system_prompt = prompt_builder.build_system_prompt(question).await;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
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
    pub async fn clear_all_caches(&self) {
        if let Some(cache) = &self.schema_cache {
            cache.clear();
        }
        if let Some(cache) = &self.compile_cache {
            cache.clear().await;
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
        if let Some(cache) = self.compile_cache.clone()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                if let Err(e) = cache.persist().await {
                    tracing::warn!(target: "vlorql", "failed to persist compile cache: {e}");
                }
            });
        }
        if let Some(guard) = self.telemetry_guard.take() {
            vlorql_core::observability::shutdown_telemetry(guard);
        }
    }
}

#[cfg(test)]
mod tests;
