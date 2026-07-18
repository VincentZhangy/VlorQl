# VlorQl 可观测性指南

本文档介绍如何启用和使用 VlorQl 的 OpenTelemetry 追踪、指标和结构化日志。

---

## 1. 架构

```
┌──────────────────────────────────────────────────────────────────────┐
│                      VlorQl 可观测性栈                                │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    应用层                                     │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐  │   │
│  │  │  查询    │─▶│  验证    │─▶│  优化    │─▶│  编译       │  │   │
│  │  │  Span    │  │  Span    │  │  Span    │  │  Span       │  │   │
│  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └──────┬──────┘  │   │
│  │       └──────────────┼──────────────┼────────────────┘        │   │
│  │                      ▼              ▼                          │   │
│  │  ┌──────────────────────────────────────────────────────┐     │   │
│  │  │   LLM Span   │   Cache Span      │   日志          │     │   │
│  │  └──────────────────────────────────────────────────────┘     │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                  │                                      │
│                                  ▼                                      │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │              OpenTelemetry SDK (Rust)                            │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │   │
│  │  │  追踪 API   │  │ 指标 API    │  │ 日志桥接 (tracing)      │  │   │
│  │  └─────────────┘  └─────────────┘  └─────────────────────────┘  │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                  │                                      │
│                                  ▼ (OTLP gRPC)                         │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                  后端 (Jaeger / Prometheus)                       │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────────────────────┐   │   │
│  │  │  Jaeger  │  │  Tempo   │  │  Prometheus / Thanos         │   │   │
│  │  │ (追踪)   │  │ (追踪)   │  │  (指标)                      │   │   │
│  │  └──────────┘  └──────────┘  └──────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

## 2. 快速开始

### 2.1 启动本地后端

```bash
# 启动 Jaeger + Prometheus
docker compose -f docker-compose.observability.yml up -d

# 验证 Jaeger 是否正在运行
curl -s http://localhost:16686/api/services | jq .

# 验证 Prometheus 是否正在运行
curl -s http://localhost:9090/api/v1/status/buildinfo | jq .
```

### 2.2 运行可观测性示例

```bash
cargo run --example with_observability --quiet
```

该示例：
1. 调用 `init_telemetry("vlorql-example", "http://localhost:4317")` 连接到 Jaeger。
2. 创建 `VlorqMetrics` 句柄用于业务指标。
3. 使用模拟 LLM 客户端构建 `VlorQl` facade。
4. 运行一个查询，通过 OTLP 导出 span 和指标。
5. 调用 `shutdown_telemetry` 在退出前刷新所有数据。

### 2.3 在 Jaeger 中查看追踪

在浏览器中打开 [http://localhost:16686](http://localhost:16686)：

1. 从下拉列表中选择服务 `vlorql-example`。
2. 点击 **Find Traces**。
3. 点击一个追踪查看 span 树：

```
vlorql.query
├── llm.generate_plan
├── vlorql.validate
└── vlorql.compile
```

### 2.4 在 Prometheus 中查询指标

打开 [http://localhost:9090](http://localhost:9090) 并尝试：

```promql
# 总查询数
vlorql_queries_total

# 查询持续时间直方图
histogram_quantile(0.95, rate(vlorql_query_duration_bucket[5m]))

# 活跃查询数
vlorql_queries_active

# 缓存命中 vs 未命中
rate(vlorql_cache_hits_total[5m]) / rate(vlorql_cache_misses_total[5m])
```

---

## 3. 探针参考

### 3.1 Span

| Span 名称 | 位置 | 关键属性 |
|-----------|------|----------|
| `vlorql.query` | `VlorQl::query` | `question_len`、`dialect`、`policy_enabled` |
| `vlorql.validate` | `VlorQl::validate_only` | `plan_has_cte` |
| `vlorql.optimize` | `QueryOptimizer::optimize_async` | `join_reorder_enabled` |
| `vlorql.compile` | `VlorQl::compile_only` | `dialect` |
| `llm.generate_plan` | `OpenAIClient::generate_plan` | `provider`、`model`、`prompt_len`、`streaming` |
| `llm.stream_plan` | `OpenAIClient::stream_plan` | `provider`、`model`、`prompt_len`、`streaming` |

### 3.2 指标

| 指标名称 | 类型 | 描述 |
|----------|------|------|
| `vlorql.queries.total` | Counter | 已启动的查询总数 |
| `vlorql.query.duration` | Histogram | 端到端查询持续时间（秒） |
| `vlorql.errors.total` | Counter | 错误数，按 `error_type` 标记 |
| `vlorql.llm.duration` | Histogram | LLM 调用持续时间（秒） |
| `vlorql.cache.hits` | Counter | 编译缓存命中次数 |
| `vlorql.cache.misses` | Counter | 编译缓存未命中次数 |
| `vlorql.queries.active` | UpDownCounter | 当前正在处理的查询数 |

### 3.3 关键事件（日志）

| 事件 | 级别 | 位置 |
|------|------|------|
| `Building SQL from QueryPlan` | DEBUG | `QueryBuilder::build` |
| `LLM response received` | DEBUG | `OpenAIClient::generate_plan` |
| `Compiled SQL length: N chars` | DEBUG | `VlorQl::compile_only` |
| `Validation failed with N errors` | ERROR | `VlorQl::query`（重试循环） |
| `Error response generated for <code>` | ERROR | `VlorQLError::to_error_response` |
| 编译缓存 HIT / MISS | DEBUG | `CompileCache::get` |
| 提示缓存 HIT / MISS | DEBUG | `PromptCache::get` |

---

## 4. 配置

### 4.1 环境变量

| 变量 | 描述 | 默认值 |
|------|------|--------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC 端点 | `http://localhost:4317` |
| `OTEL_SERVICE_NAME` | 发送到后端的服务名称 | `vlorql` |
| `RUST_LOG` | 追踪日志过滤器 | `vlorql=info,info` |
| `OTEL_TRACES_SAMPLER` | 采样策略 | `parentbased_traceid_ratio` |

### 4.2 编程式设置

```rust
use vlorql_core::observability::{init_telemetry, shutdown_telemetry, VlorqMetrics};
use std::sync::Arc;

// 1. 初始化 OTLP 导出器。
let guard = init_telemetry("my-service", "http://localhost:4317")?;

// 2. 创建指标（使用全局 meter）。
let metrics = Arc::new(VlorqMetrics::new());

// 3. 将指标传递给 facade。
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect_name("postgres")
    .with_metrics(metrics)
    .build()?;

// 4. 退出时关闭。
shutdown_telemetry(guard);
```

### 4.3 采样

对于生产环境，设置 `OTEL_TRACES_SAMPLER` 来控制记录的追踪数量：

```bash
# 记录所有追踪（开发环境默认）
export OTEL_TRACES_SAMPLER=always_on

# 记录 10% 的追踪（推荐用于高流量生产环境）
export OTEL_TRACES_SAMPLER=traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1

# 不记录任何内容（紧急覆盖）
export OTEL_TRACES_SAMPLER=always_off
```

---

## 5. 生产部署

### 5.1 采样率

对于生产环境，使用概率采样器：

```rust
use opentelemetry_sdk::trace::Sampler;
use opentelemetry_sdk::Resource;

let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
    .with_sampler(Sampler::TraceIdRatioBased(0.1))
    .with_resource(Resource::builder()
        .with_attribute(KeyValue::new("service.name", "vlorql-prod"))
        .build())
    // ... 导出器设置 ...
    .build();
```

### 5.2 OTLP Collector

对于生产环境，在应用程序和后端之间部署 [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/)：

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

### 5.3 资源属性

添加自定义资源属性以标识部署：

```rust
opentelemetry_sdk::Resource::builder()
    .with_attribute(KeyValue::new("service.name", "vlorql-prod"))
    .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
    .with_attribute(KeyValue::new("deployment.environment", "production"))
    .build();
```

---

## 6. 故障排除

### Span 没有出现在 Jaeger 中

1. 验证 OTLP 端点是否可达：
   ```bash
   grpcurl -plaintext localhost:4317 list
   ```

2. 检查 `init_telemetry` 返回 `Ok` 并且 guard 没有被提前丢弃。

3. 确保在进程退出前调用了 `shutdown_telemetry` 以使批量导出器刷新。

### 指标没有出现在 Prometheus 中

1. 验证 `prometheus.yml` 中的 Prometheus 目标配置是否正确。

2. 如果使用 OTLP → Prometheus 路径，请部署一个带有 Prometheus 导出器的 OpenTelemetry Collector。

3. 检查 `VlorqMetrics::new()` 是否在 `opentelemetry::global::set_meter_provider(...)` 之后调用。

### 日志没有与追踪关联

`tracing-opentelemetry` 层会自动将 `trace_id` 和 `span_id` 注入到每个事件中。使用以下方式安装 subscriber：

```rust
tracing_subscriber::registry()
    .with(env_filter)
    .with(tracing_subscriber::fmt::layer().json())
    .with(tracing_opentelemetry::layer().with_tracer(tracer))
    .init();
```

---

## 7. 参见

- [`guide.md`](./guide.md) — 用户指南
- [`deployment.md`](./deployment.md) — 部署指南
- [`docker-compose.observability.yml`](../docker-compose.observability.yml) — 本地 Jaeger + Prometheus
- [OpenTelemetry Rust 文档](https://docs.rs/opentelemetry)