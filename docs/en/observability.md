# VlorQl Observability Guide

This document describes how to enable and use OpenTelemetry tracing,
metrics, and structured logging with VlorQl.

---

## 1. Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                      VlorQl Observability Stack                      │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    Application Layer                         │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐  │   │
│  │  │  Query   │─▶│ Validate │─▶│ Optimize │─▶│  Compile    │  │   │
│  │  │  Span    │  │  Span    │  │  Span    │  │  Span       │  │   │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬──────┘  │   │
│  │       └──────────────┼──────────────┼────────────────┘        │   │
│  │                      ▼              ▼                          │   │
│  │  ┌──────────────────────────────────────────────────────┐     │   │
│  │  │      LLM Span     │    Cache Span     │    Logs     │     │   │
│  │  └──────────────────────────────────────────────────────┘     │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                  │                                      │
│                                  ▼                                      │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │              OpenTelemetry SDK (Rust)                            │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │   │
│  │  │  Trace API  │  │ Metrics API │  │ Logs Bridge (tracing)   │  │   │
│  │  └─────────────┘  └─────────────┘  └─────────────────────────┘  │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                  │                                      │
│                                  ▼ (OTLP gRPC)                         │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                  Backend (Jaeger / Prometheus)                   │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────────────────┐   │   │
│  │  │  Jaeger  │  │  Tempo   │  │  Prometheus / Thanos         │   │   │
│  │  │ (Traces) │  │ (Traces) │  │  (Metrics)                   │   │   │
│  │  └──────────┘  └──────────┘  └──────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. Quick start

### 2.1 Start the local backends

```bash
# Start Jaeger + Prometheus
docker compose -f docker-compose.observability.yml up -d

# Verify Jaeger is running
curl -s http://localhost:16686/api/services | jq .

# Verify Prometheus is running
curl -s http://localhost:9090/api/v1/status/buildinfo | jq .
```

### 2.2 Run the observability example

```bash
cargo run --example with_observability --quiet
```

This example:
1. Calls `init_telemetry("vlorql-example", "http://localhost:4317")` to connect to Jaeger.
2. Creates a `VlorqMetrics` handle for business metrics.
3. Builds a `VlorQl` facade with a mock LLM client.
4. Runs a query, which exports spans and metrics via OTLP.
5. Calls `shutdown_telemetry` to flush all data before exiting.

### 2.3 View traces in Jaeger

Open [http://localhost:16686](http://localhost:16686) in your browser:

1. Select service `vlorql-example` from the dropdown.
2. Click **Find Traces**.
3. Click on a trace to see the span tree:

```
vlorql.query
├── llm.generate_plan
├── vlorql.validate
└── vlorql.compile
```

### 2.4 Query metrics in Prometheus

Open [http://localhost:9090](http://localhost:9090) and try:

```promql
# Total queries
vlorql_queries_total

# Query duration histogram
histogram_quantile(0.95, rate(vlorql_query_duration_bucket[5m]))

# Active queries
vlorql_queries_active

# Cache hits vs misses
rate(vlorql_cache_hits_total[5m]) / rate(vlorql_cache_misses_total[5m])
```

---

## 3. Instrumentation reference

### 3.1 Spans

| Span name              | Location                          | Key attributes                                              |
|------------------------|-----------------------------------|-------------------------------------------------------------|
| `vlorql.query`         | `VlorQl::query`                   | `question_len`, `dialect`, `policy_enabled`                 |
| `vlorql.validate`      | `VlorQl::validate_only`           | `plan_has_cte`                                              |
| `vlorql.optimize`      | `QueryOptimizer::optimize_async`  | `join_reorder_enabled`                                      |
| `vlorql.compile`       | `VlorQl::compile_only`            | `dialect`                                                   |
| `llm.generate_plan`    | `OpenAIClient::generate_plan`     | `provider`, `model`, `prompt_len`, `streaming`              |
| `llm.stream_plan`      | `OpenAIClient::stream_plan`       | `provider`, `model`, `prompt_len`, `streaming`              |

### 3.2 Metrics

| Metric name                 | Type             | Description                        |
|-----------------------------|------------------|------------------------------------|
| `vlorql.queries.total`      | Counter          | Total queries started              |
| `vlorql.query.duration`     | Histogram        | End-to-end query duration (s)      |
| `vlorql.errors.total`       | Counter          | Errors, tagged by `error_type`     |
| `vlorql.llm.duration`       | Histogram        | LLM call duration (s)              |
| `vlorql.cache.hits`         | Counter          | Compile cache hits                 |
| `vlorql.cache.misses`       | Counter          | Compile cache misses               |
| `vlorql.queries.active`     | UpDownCounter    | Currently in-flight queries        |

### 3.3 Key events (logs)

| Event                                     | Level   | Location                              |
|-------------------------------------------|---------|---------------------------------------|
| `Building SQL from QueryPlan`             | DEBUG   | `QueryBuilder::build`                 |
| `LLM response received`                   | DEBUG   | `OpenAIClient::generate_plan`         |
| `Compiled SQL length: N chars`            | DEBUG   | `VlorQl::compile_only`                |
| `Validation failed with N errors`         | ERROR   | `VlorQl::query` (retry loop)          |
| `Error response generated for <code>`     | ERROR   | `VlorQLError::to_error_response`      |
| Compile cache HIT / MISS                  | DEBUG   | `CompileCache::get`                   |
| Prompt cache HIT / MISS                   | DEBUG   | `PromptCache::get`                    |

---

## 4. Configuration

### 4.1 Environment variables

| Variable                     | Description                          | Default                     |
|------------------------------|--------------------------------------|-----------------------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint                   | `http://localhost:4317`     |
| `OTEL_SERVICE_NAME`          | Service name sent to the backend     | `vlorql`                   |
| `RUST_LOG`                   | Tracing log filter                   | `vlorql=info,info`         |
| `OTEL_TRACES_SAMPLER`        | Sampling strategy                    | `parentbased_traceid_ratio` |

### 4.2 Programmatic setup

```rust
use vlorql_core::observability::{init_telemetry, shutdown_telemetry, VlorqMetrics};
use std::sync::Arc;

// 1. Initialise OTLP exporters.
let guard = init_telemetry("my-service", "http://localhost:4317")?;

// 2. Create metrics (uses the global meter).
let metrics = Arc::new(VlorqMetrics::new());

// 3. Pass metrics to the facade.
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_metrics(metrics)
    .build()?;

// 4. Shut down on exit.
shutdown_telemetry(guard);
```

### 4.3 Sampling

For production, set `OTEL_TRACES_SAMPLER` to control how many traces
are recorded:

```bash
# Record all traces (default for development)
export OTEL_TRACES_SAMPLER=always_on

# Record 10% of traces (recommended for high-traffic production)
export OTEL_TRACES_SAMPLER=traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1

# Record nothing (emergency override)
export OTEL_TRACES_SAMPLER=always_off
```

---

## 5. Production deployment

### 5.1 Sampling rate

For production, use a probabilistic sampler:

```rust
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::Resource;

let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
    .with_sampler(Sampler::TraceIdRatioBased(0.1))
    .with_resource(Resource::builder()
        .with_attribute(KeyValue::new("service.name", "vlorql-prod"))
        .build())
    // ... exporter setup ...
    .build();
```

### 5.2 OTLP Collector

For production, deploy an [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/)
between the application and the backend:

```yaml
# otel-collector-config.yml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317

processors:
  batch:
    timeout: 1s
    send_batch_size: 1024
  memory_limiter:
    check_interval: 1s
    limit_mib: 512

exporters:
  otlp:
    endpoint: jaeger:4317
    tls:
      insecure: true
  prometheus:
    endpoint: 0.0.0.0:8889

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [memory_limiter, batch]
      exporters: [otlp]
    metrics:
      receivers: [otlp]
      processors: [memory_limiter, batch]
      exporters: [prometheus]
```

### 5.3 Resource attributes

Add custom resource attributes to identify the deployment:

```rust
opentelemetry_sdk::Resource::builder()
    .with_attribute(KeyValue::new("service.name", "vlorql-prod"))
    .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
    .with_attribute(KeyValue::new("deployment.environment", "production"))
    .build();
```

---

## 6. Troubleshooting

### Spans are not appearing in Jaeger

1. Verify the OTLP endpoint is reachable:
   ```bash
   grpcurl -plaintext localhost:4317 list
   ```

2. Check that `init_telemetry` returns `Ok` and the guard is not dropped
   prematurely.

3. Ensure `shutdown_telemetry` is called so the batch exporter flushes
   before the process exits.

### Metrics are not appearing in Prometheus

1. Verify the Prometheus target is configured correctly in
   `prometheus.yml`.

2. If using the OTLP → Prometheus path, deploy an OpenTelemetry
   Collector with the Prometheus exporter.

3. Check that `VlorqMetrics::new()` is called after
   `opentelemetry::global::set_meter_provider(...)`.

### Logs are not correlated with traces

The `tracing-opentelemetry` layer automatically injects `trace_id`
and `span_id` into every event. Install the subscriber with:

```rust
tracing_subscriber::registry()
    .with(env_filter)
    .with(tracing_subscriber::fmt::layer().json())
    .with(tracing_opentelemetry::layer().with_tracer(tracer))
    .init();
```

---

## 7. See also

- [`guide.md`](./guide.md) — user guide
- [`deployment.md`](./deployment.md) — deployment guide
- [`docker-compose.observability.yml`](../docker-compose.observability.yml) — local Jaeger + Prometheus
- [OpenTelemetry Rust docs](https://docs.rs/opentelemetry)