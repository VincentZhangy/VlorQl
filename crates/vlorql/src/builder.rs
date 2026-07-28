use serde_json::json;
use std::sync::Arc;
use vlorql_core::cache::{CompileCache, LlmResponseCache, PromptCache, SchemaCache};
use vlorql_core::compile::{RewriteEngine, SqlCompiler, get_compiler};
use vlorql_core::errors::{ConfigErrorKind, VlorQLError};
use vlorql_core::execute::DatabaseExecutor;
use vlorql_core::observability::{TelemetryGuard, VlorqMetrics, init_telemetry};
use vlorql_core::optimizer::QueryOptimizer;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::schema::{
    ArcSchemaSnapshot, DialectProfile, IdentifierQuoting, SchemaSnapshot, SqlDialect,
};
use vlorql_core::statistics::StatisticsProvider;
use vlorql_llm::{LlmClient, LlmConfig, create_llm_client};
use crate::{VlorQl, DEFAULT_MAX_RETRIES};

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
/// assert_eq!(vlorql.max_retries(), 3);
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
    schema_cache_config: Option<(u64, u64)>,
    compile_cache: Option<Arc<CompileCache>>,
    prompt_cache: Option<Arc<PromptCache>>,
    llm_cache: Option<LlmResponseCache>,
    telemetry_endpoint: Option<String>,
    telemetry_guard: Option<TelemetryGuard>,
    metrics: Option<Arc<VlorqMetrics>>,
    executor: Option<Arc<dyn DatabaseExecutor>>,
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
            schema_cache_config: None,
            compile_cache: None,
            prompt_cache: None,
            llm_cache: None,
            telemetry_endpoint: None,
            telemetry_guard: None,
            metrics: None,
            executor: None,
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
        self.schema_cache_config = Some((capacity, ttl_seconds));
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

    /// Configures an [`LlmResponseCache`] with the given capacity and TTL.
    ///
    /// Caches LLM-generated [`QueryPlan`] values keyed by question text,
    /// schema version, and model fingerprint, avoiding redundant LLM
    /// invocations for identical questions.
    #[must_use]
    pub fn with_llm_cache(mut self, max_entries: u64, ttl_seconds: u64) -> Self {
        self.llm_cache = Some(LlmResponseCache::new(max_entries, ttl_seconds));
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

    /// Supplies a [`DatabaseExecutor`] that can run compiled SQL queries.
    ///
    /// When an executor is configured, you can use [`VlorQl::run`] to
    /// perform the full plan → validate → compile → execute pipeline
    /// in a single call.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use vlorql::execute::PgExecutor;
    /// use std::sync::Arc;
    ///
    /// let (client, connection) = tokio_postgres::connect(
    ///     "host=localhost user=postgres", tokio_postgres::NoTls,
    /// )
    /// .await
    /// .expect("connect");
    /// tokio::spawn(async move { connection.await.expect("connection") });
    ///
    /// let vlorql = VlorQl::builder()
    ///     .with_schema(schema)
    ///     .with_dialect_name("postgres")
    ///     .with_executor(Arc::new(PgExecutor::new(client)))
    ///     .build()
    ///     .expect("facade");
    /// ```
    #[must_use]
    pub fn with_executor(mut self, executor: Arc<dyn DatabaseExecutor>) -> Self {
        self.executor = Some(executor);
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

        // Build the schema cache (deferred so metrics — if any — are available).
        let schema_cache = self.schema_cache_config.map(|(capacity, ttl_seconds)| {
            Arc::new(SchemaCache::new(
                capacity,
                ttl_seconds,
                self.metrics.clone(),
            ))
        });

        Ok(VlorQl {
            schema,
            dialect,
            policy: self.policy,
            compiler: Arc::from(compiler),
            rewrite_engine: self.rewrite_engine,
            llm_client,
            max_retries: self.max_retries,
            optimizer,
            schema_cache,
            compile_cache: self.compile_cache,
            prompt_cache: self.prompt_cache,
            llm_cache: self
                .llm_cache
                .unwrap_or_else(|| LlmResponseCache::new(1000, 3600)),
            telemetry_guard: self.telemetry_guard,
            metrics: self.metrics,
            executor: self.executor,
        })
    }
}

fn parse_dialect_name(name: &str) -> Result<DialectProfile, VlorQLError> {
    let normalized = name.trim().to_ascii_lowercase();
    let dialect = match normalized.as_str() {
        "postgres" | "postgresql" => SqlDialect::Postgres,
        "sqlite" => SqlDialect::Sqlite,
        "mysql" | "my_sql" => SqlDialect::MySql,
        _ => {
            return Err(VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: name.to_owned(),
                },
                json!({"accepted": ["postgres", "sqlite", "mysql"]}),
            ));
        }
    };

    let quote_style = if dialect == SqlDialect::MySql {
        IdentifierQuoting::Backtick
    } else {
        IdentifierQuoting::DoubleQuote
    };
    Ok(DialectProfile {
        dialect,
        quote_style,
        ..DialectProfile::default()
    })
}
