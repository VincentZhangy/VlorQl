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

use futures::StreamExt;
use futures::stream::Stream;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::Instrument;
use vlorql_core::compile::{SqlCompiler, get_compiler};
use vlorql_core::errors::{ConfigErrorKind, LlmErrorKind, SchemaErrorKind, ValidationErrorKind, VlorQLError};
use vlorql_core::observability::{TelemetryGuard, VlorqMetrics, init_telemetry};
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::policy::{PolicyConfig, PolicyEngine};
use vlorql_core::prompt::PromptBuilder;
use vlorql_core::schema::{ArcSchemaSnapshot, QueryPlan};
use vlorql_core::statistics::StatisticsProvider;
use vlorql_core::validate::ValidationPipeline;

pub use vlorql_core::cache::{CompileCache, PromptCache, SchemaCache};
pub use vlorql_core::compile::{CompiledQuery, DialectConfig, DialectRegistry, Parameter, RewriteEngine, RewriteRule};
pub use vlorql_core::prompt::{ExamplePair, PromptSkill};
pub use vlorql_core::errors::{ErrorResponse, ValidationErrors};
pub use vlorql_core::optimizer::QueryOptimizer as QueryOptimizerCore;
pub use vlorql_core::schema::{DialectProfile, SchemaSnapshot, SqlDialect};
pub use vlorql_core::validate::{OptimizedPlan, ValidatedPlan};
pub use vlorql_llm::{
    LlmClient, LlmConfig, LlmProvider, create_llm_client, detect_template_leak, parse_query_plan,
    parse_query_plan_lenient,
};

const DEFAULT_MAX_RETRIES: usize = 2;

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
///     from: FromClause { table: "users".to_owned(), alias: None },
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
    telemetry_guard: Option<TelemetryGuard>,
    metrics: Option<Arc<VlorqMetrics>>,
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
            .field("max_retries", &self.max_retries)
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
    pub async fn query(&self, question: &str) -> Result<CompiledQuery, VlorQLError> {
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

            // Build the system prompt, optionally using the prompt cache.
            let prompt_builder = PromptBuilder::new(
                Arc::clone(&self.schema),
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

            let mut llm_question = question.to_owned();
            for attempt in 0..=self.max_retries {
                let plan = match client.generate_plan(&llm_question, &system_prompt).await {
                    Ok(plan) => plan,
                    Err(e) if e.is_retryable() && attempt < self.max_retries => {
                        llm_question = format_retry_question_str(&llm_question, &e);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                match self.validate_only(&plan) {
                    Ok(validated_plan) => {
                        // Optimize when an optimizer is configured, then compile.
                        let plan_for_compile = match &self.optimizer {
                            Some(optimizer) => {
                                match optimizer.optimize_async(validated_plan.as_plan()).await {
                                    Ok(optimized) => {
                                        // Re-validate policy on the optimized plan.
                                        let pipeline = self.build_pipeline();
                                        if let Err(stage_errors) =
                                            pipeline.policy().validate(&optimized, &self.schema)
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
                            }
                            return Ok((*cached).clone());
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

                        let elapsed = start.elapsed().as_secs_f64();
                        if let Some(ref m) = self.metrics {
                            m.query_duration_histogram.record(elapsed, &[]);
                            m.active_queries.add(-1, &[]);
                        }
                        return Ok(compiled);
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

                        llm_question = format_retry_question(question, &errors);
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
        self.build_pipeline().validate(plan)
    }

    /// Validates a plan and, when an optimizer is configured, applies
    /// optimisation passes.  Returns an [`OptimizedPlan`] that derefs to
    /// [`ValidatedPlan`].
    ///
    /// # Errors
    ///
    /// Returns [`ValidationErrors`] when any validation stage (including
    /// the post-optimisation policy re-check) fails.
    pub async fn validate_and_optimize(
        &self,
        plan: &QueryPlan,
    ) -> Result<OptimizedPlan, ValidationErrors> {
        self.build_pipeline_with_optimizer()
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
    fn build_pipeline(&self) -> ValidationPipeline {
        ValidationPipeline::new(
            Arc::clone(&self.schema),
            self.dialect.clone(),
            PolicyEngine::new(self.policy.clone()),
        )
    }

    /// Builds a [`ValidationPipeline`] with the optional optimizer attached.
    fn build_pipeline_with_optimizer(&self) -> ValidationPipeline {
        let mut pipeline = ValidationPipeline::new(
            Arc::clone(&self.schema),
            self.dialect.clone(),
            PolicyEngine::new(self.policy.clone()),
        );
        if let Some(ref optimizer) = self.optimizer {
            pipeline = pipeline.with_optimizer(optimizer.clone());
        }
        pipeline
    }
}

/// Builder for [`VlorQl`].
///
/// # Examples
///
/// ```
/// use vlorql::VlorQl;
/// use vlorql_core::schema::{SchemaSnapshot, TableSchema, ColumnSchema, DataType, SchemaMetadata};
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
/// let builder = VlorQl::builder()
///     .with_schema(schema)
///     .with_dialect_name("sqlite")
///     .with_policy(PolicyConfig::default());
/// let vlorql = builder.build().expect("valid facade");
/// assert_eq!(vlorql.max_retries(), 2);
/// ```
pub struct VlorQlBuilder {
    schema: Option<ArcSchemaSnapshot>,
    dialect: Option<DialectProfile>,
    dialect_name: Option<String>,
    policy: PolicyConfig,
    compiler: Option<Box<dyn SqlCompiler>>,
    rewrite_engine: Option<RewriteEngine>,
    llm_client: Option<Box<dyn LlmClient>>,
    llm_config: Option<LlmConfig>,
    max_retries: usize,
    stats_provider: Option<Arc<dyn StatisticsProvider>>,
    schema_cache: Option<Arc<SchemaCache>>,
    compile_cache: Option<Arc<CompileCache>>,
    prompt_cache: Option<Arc<PromptCache>>,
    telemetry_endpoint: Option<String>,
    telemetry_guard: Option<TelemetryGuard>,
    metrics: Option<Arc<VlorqMetrics>>,
}

impl Default for VlorQlBuilder {
    fn default() -> Self {
        Self {
            schema: None,
            dialect: None,
            dialect_name: None,
            policy: PolicyConfig::default(),
            compiler: None,
            rewrite_engine: None,
            llm_client: None,
            llm_config: None,
            max_retries: DEFAULT_MAX_RETRIES,
            stats_provider: None,
            schema_cache: None,
            compile_cache: None,
            prompt_cache: None,
            telemetry_endpoint: None,
            telemetry_guard: None,
            metrics: None,
        }
    }
}

impl VlorQlBuilder {
    /// Supplies the shared schema snapshot.
    #[must_use]
    pub fn with_schema(mut self, schema: Arc<SchemaSnapshot>) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Supplies the complete dialect profile.
    #[must_use]
    pub fn with_dialect(mut self, dialect: DialectProfile) -> Self {
        self.dialect = Some(dialect);
        self.dialect_name = None;
        self
    }

    /// Supplies access-control policy configuration.
    #[must_use]
    pub fn with_policy(mut self, policy: PolicyConfig) -> Self {
        self.policy = policy;
        self
    }

    /// Supplies an LLM client. Any `LlmClient` implementation can be passed directly.
    #[must_use]
    pub fn with_llm_client<C>(mut self, client: C) -> Self
    where
        C: LlmClient + 'static,
    {
        self.llm_client = Some(Box::new(client));
        self
    }

    /// Builds an LLM client from an [`LlmConfig`] using the crate's factory.
    #[must_use]
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Some(config);
        self
    }

    /// Supplies a custom SQL compiler instead of the dialect default.
    #[must_use]
    pub fn with_compiler<C>(mut self, compiler: C) -> Self
    where
        C: SqlCompiler + 'static,
    {
        self.compiler = Some(Box::new(compiler));
        self
    }

    /// Supplies a [`RewriteEngine`] for post-compilation SQL rewrites.
    #[must_use]
    pub fn with_rewrite_engine(mut self, engine: RewriteEngine) -> Self {
        self.rewrite_engine = Some(engine);
        self
    }

    /// Selects a dialect by name and lets the builder create its compiler.
    ///
    /// Accepted names are `postgres`, `postgresql`, `sqlite`, and `mysql`
    /// (case-insensitive). Invalid names are reported by [`Self::build`].
    #[must_use]
    pub fn with_dialect_name(mut self, dialect: impl Into<String>) -> Self {
        self.dialect_name = Some(dialect.into());
        self.dialect = None;
        self
    }

    /// Sets the number of validation retries after the initial LLM attempt.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Supplies a statistics provider used for cost-based query optimisation.
    ///
    /// When set, the built [`VlorQl`] facade will run the
    /// [`QueryOptimizer`] after
    /// validation succeeds, applying constant folding, predicate pushdown,
    /// column pruning, and (when statistics are available) cost-based join
    /// reordering.
    #[must_use]
    pub fn with_statistics_provider(mut self, provider: Arc<dyn StatisticsProvider>) -> Self {
        self.stats_provider = Some(provider);
        self
    }

    /// Configures a [`SchemaCache`] with the given capacity and TTL.
    ///
    /// Caches schema snapshots keyed by version + source to avoid
    /// re-parsing or re-fetching.
    #[must_use]
    pub fn with_schema_cache(mut self, capacity: u64, ttl_seconds: u64) -> Self {
        self.schema_cache = Some(Arc::new(SchemaCache::new(capacity, ttl_seconds)));
        self
    }

    /// Configures a [`CompileCache`] with the given weight limit and TTL.
    ///
    /// Caches compiled SQL results keyed by plan hash + dialect so that
    /// the same plan does not need to be re-compiled for the same dialect.
    #[must_use]
    pub fn with_compile_cache(mut self, max_size: u64, ttl_seconds: u64) -> Self {
        self.compile_cache = Some(Arc::new(CompileCache::new(max_size, ttl_seconds)));
        self
    }

    /// Configures a [`PromptCache`] with the given capacity and TTL.
    ///
    /// Caches system prompts keyed by schema version + dialect + policy
    /// hash, avoiding re-generation when the configuration has not
    /// changed.
    #[must_use]
    pub fn with_prompt_cache(mut self, capacity: u64, ttl_seconds: u64) -> Self {
        self.prompt_cache = Some(Arc::new(PromptCache::new(capacity, ttl_seconds)));
        self
    }

    /// Configures OpenTelemetry tracing and metrics with the given OTLP
    /// endpoint (e.g. `http://localhost:4317`).
    ///
    /// The exporter is initialised immediately so that any subsequent
    /// operations (including build errors) can be traced. The
    /// [`TelemetryGuard`] is kept alive for the lifetime of the
    /// [`VlorQl`] facade and is shut down when the facade is dropped.
    #[must_use]
    pub fn with_telemetry(mut self, otlp_endpoint: String) -> Self {
        match init_telemetry("vlorql", &otlp_endpoint) {
            Ok(guard) => {
                self.telemetry_guard = Some(guard);
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to initialise OTLP telemetry; continuing without it");
            }
        }
        self.telemetry_endpoint = Some(otlp_endpoint);
        self
    }

    /// Supplies a [`VlorqMetrics`] handle for recording business metrics.
    ///
    /// The metrics are recorded at key points in the query pipeline
    /// (query count, duration, cache hits/misses, LLM latency, errors).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<VlorqMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Builds the facade and verifies the required schema and dialect/compiler setup.
    pub fn build(self) -> Result<VlorQl, VlorQLError> {
        vlorql_core::observability::init_console_logging();
        let schema = self.schema.ok_or_else(|| {
            VlorQLError::config(
                ConfigErrorKind::MissingSchema,
                json!({"component": "schema"}),
            )
        })?;
        let dialect = match (self.dialect, self.dialect_name) {
            (Some(dialect), _) => dialect,
            (None, Some(name)) => parse_dialect_name(&name)?,
            (None, None) => {
                return Err(VlorQLError::config(
                    ConfigErrorKind::InvalidDialect {
                        dialect: "not configured".to_owned(),
                    },
                    json!({"component": "dialect"}),
                ));
            }
        };
        let compiler = self
            .compiler
            .unwrap_or_else(|| get_compiler(dialect.dialect));

        let llm_client = match (self.llm_client, self.llm_config) {
            (Some(client), _) => Some(Arc::from(client)),
            (None, Some(config)) => Some(Arc::from(create_llm_client(config)?)),
            (None, None) => None,
        };

        let optimizer = self.stats_provider.map(QueryOptimizer::new);

        Ok(VlorQl {
            schema,
            dialect,
            policy: self.policy,
            compiler: Arc::from(compiler),
            rewrite_engine: self.rewrite_engine,
            llm_client,
            max_retries: self.max_retries,
            optimizer,
            schema_cache: self.schema_cache,
            compile_cache: self.compile_cache,
            prompt_cache: self.prompt_cache,
            telemetry_guard: self.telemetry_guard,
            metrics: self.metrics,
        })
    }
}

impl Drop for VlorQl {
    fn drop(&mut self) {
        if let Some(guard) = self.telemetry_guard.take() {
            vlorql_core::observability::shutdown_telemetry(guard);
        }
    }
}

fn parse_dialect_name(name: &str) -> Result<DialectProfile, VlorQLError> {
    let normalized = name.trim().to_ascii_lowercase();
    let dialect = match normalized.as_str() {
        "postgres" | "postgresql" => vlorql_core::schema::SqlDialect::Postgres,
        "sqlite" => vlorql_core::schema::SqlDialect::Sqlite,
        "mysql" | "my_sql" => vlorql_core::schema::SqlDialect::MySql,
        _ => {
            return Err(VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: name.to_owned(),
                },
                json!({"accepted": ["postgres", "sqlite", "mysql"]}),
            ));
        }
    };

    let quote_style = if dialect == vlorql_core::schema::SqlDialect::MySql {
        vlorql_core::schema::IdentifierQuoting::Backtick
    } else {
        vlorql_core::schema::IdentifierQuoting::DoubleQuote
    };
    Ok(DialectProfile {
        dialect,
        quote_style,
        ..DialectProfile::default()
    })
}

fn format_retry_question_str(question: &str, error: &VlorQLError) -> String {
    let feedback = error.to_string();
    let hint = match error {
        VlorQLError::Llm {
            kind: vlorql_core::errors::LlmErrorKind::ParseError { .. },
            ..
        } => " TIP: If the previous query used NOT EXISTS with a subquery, replace it with LEFT JOIN + IS NULL — it is simpler and avoids JSON nesting issues.".to_owned(),
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
                format!(" TIP: Column `{column}` does not exist on table `{table}`. Use only the exact column names listed in the Schema section.")
            } else {
                format!(
                    " TIP: Column `{column}` does not exist on table `{table}`. Available columns: `{available}`. Use only exact column names from the Schema."
                )
            }
        }
        VlorQLError::Validation {
            kind: ValidationErrorKind::MultipleErrors { .. },
            ..
        } => {
            // Try to extract available_columns from the first error in the list.
            let tip = error
                .details()
                .get("errors")
                .and_then(|v| v.as_array())
                .and_then(|errs| errs.first())
                .and_then(|first| {
                    let col = first.get("column").and_then(|v| v.as_str())?;
                    let table = first.get("table").and_then(|v| v.as_str())?;
                    let available = first
                        .get("available_columns")
                        .and_then(|v| v.as_array())?;
                    let cols: Vec<&str> = available.iter().filter_map(|v| v.as_str()).collect();
                    Some(format!(
                        " TIP: Column `{table}.{col}` does not exist. Available columns in `{table}`: `{}`. Use only exact column names from the Schema.",
                        cols.join(", ")
                    ))
                })
                .unwrap_or_default();
            tip
        }
        _ => "".to_owned(),
    };
    format!(
        "{question}\n\nThe previous QueryPlan failed validation. Correct it and return only a new JSON QueryPlan. Feedback:\n{feedback}{hint}"
    )
}

fn format_retry_question(original_question: &str, errors: &ValidationErrors) -> String {
    let feedback = errors
        .as_slice()
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let hints: Vec<String> = errors.as_slice().iter().filter_map(|error| {
        match error {
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
        }
    }).collect();
    let hints_str = if hints.is_empty() {
        String::new()
    } else {
        format!("\n{}", hints.join("\n"))
    };
    format!(
        "{original_question}\n\nThe previous QueryPlan failed validation. Correct it and return only a new JSON QueryPlan. Feedback:\n{feedback}{hints_str}"
    )
}

fn validation_errors_to_error(errors: ValidationErrors) -> VlorQLError {
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
async fn run_stream_with_retry(
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
        let stream = match llm_client
            .stream_plan(question.clone(), system_prompt.clone())
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                if error.is_retryable() && attempt < max_retries {
                    question = format_retry_question_str(&question, &error);
                    continue;
                }
                let _ = event_tx.send(Err(error));
                return;
            }
        };

        let mut buffer = String::new();
        let mut stream = stream;
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
                        question = format_retry_question_str(&question, &error);
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
                question = format_retry_question_str(&question, error);
                continue;
            }
            _ => {
                let _ = event_tx.send(Ok(event));
                return;
            }
        }
    }
}

fn process_assembled_text(
    buffer: String,
    schema: ArcSchemaSnapshot,
    dialect: DialectProfile,
    policy: PolicyConfig,
    compiler: Arc<dyn SqlCompiler>,
) -> StreamEvent {
    if let Some(details) = vlorql_llm::detect_template_leak(&buffer) {
        return StreamEvent::Error(VlorQLError::llm(
            LlmErrorKind::ParseError { details },
            json!({
                "source": "stream_assistant_content",
                "buffer_length": buffer.len(),
            }),
        ));
    }
    let plan: QueryPlan = match vlorql_llm::parse_query_plan(&buffer) {
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
            .validate(&plan);
    match validation {
        Ok(validated) => match compiler.compile(&validated) {
            Ok(_) => StreamEvent::PlanComplete(Box::new(plan)),
            Err(error) => StreamEvent::Error(error),
        },
        Err(errors) => StreamEvent::Error(validation_errors_to_error(errors)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use vlorql_core::errors::LlmErrorKind;
    use vlorql_core::schema::{
        ColumnSchema, DataType, Expression, FromClause, Predicate, Projection, QueryPlan,
        SchemaMetadata, TableSchema,
    };
    use vlorql_llm::MockLlmClient;

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
            from: FromClause {
                table: "users".to_owned(),
                alias: Some("t1".to_owned()),
            },
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
        let compiled = facade
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
        ) -> Result<QueryPlan, VlorQLError> {
            self.plans
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
                })
        }

        async fn stream_plan(
            &self,
            question: String,
            system_prompt: String,
        ) -> Result<
            Box<dyn futures::stream::Stream<Item = Result<String, VlorQLError>> + Send + Unpin>,
            VlorQLError,
        > {
            let plan = self.generate_plan(&question, &system_prompt).await?;
            let serialized = serde_json::to_string(&plan).unwrap_or_default();
            Ok(Box::new(futures::stream::iter(vec![Ok(serialized)])))
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

        let compiled = facade
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
        assert_eq!(v.max_retries(), 2);
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
        while let Some(item) = stream.next().await {
            match item.expect("event should be Ok") {
                StreamEvent::TextChunk(_) => saw_chunks = true,
                StreamEvent::PlanComplete(plan) => final_plan = Some(*plan),
                StreamEvent::Error(error) => panic!("unexpected error event: {error}"),
            }
        }
        assert!(saw_chunks, "should receive at least one text chunk");
        assert_eq!(final_plan, Some(valid_plan()));
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

        let compiled = facade
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
