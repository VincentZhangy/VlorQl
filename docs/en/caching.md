# VlorQl Caching System

## Overview

VlorQl ships with three layers of pluggable caches to reduce redundant computation and I/O overhead:

| Cache | Purpose | Key |
|-------|---------|-----|
| **SchemaCache** | Caches `SchemaSnapshot` to avoid repeated parsing or fetching | `SchemaCacheKey` (version + source) |
| **CompileCache** | Caches `ValidatedPlan → CompiledQuery` to avoid repeated compilation | `CompileCacheKey` (plan_hash + dialect + quote_style) |
| **PromptCache** | Caches system prompts to avoid repeated generation | `PromptCacheKey` (schema_version + dialect + policy_hash) |

## How to Configure

### Via VlorQlBuilder

```rust
use vlorql::VlorQl;

let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect(dialect)
    .with_policy(policy)
    .with_llm_client(llm_client)
    // Enable all caches
    .with_schema_cache(10, 3600)        // 10 entries, TTL 1 hour
    .with_compile_cache(100 * 1024, 1800)  // 100 KB weight limit, TTL 30 minutes
    .with_prompt_cache(50, 7200)        // 50 entries, TTL 2 hours
    .build()?;
```

### Enable Only Selected Caches

Each cache is optional; only call the methods you need:

```rust
// Enable only the compile cache
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect(dialect)
    .with_policy(policy)
    .with_llm_client(llm_client)
    .with_compile_cache(1024, 60)
    .build()?;
```

## Cache Size and TTL Recommendations

| Cache | Recommended Capacity | Recommended TTL | Notes |
|-------|---------------------|-----------------|-------|
| SchemaCache | 10--100 | 1--24 hours | Schema rarely changes |
| CompileCache | 1000--10000 (weight) | 30 minutes -- 1 hour | Caches compiled SQL |
| PromptCache | 50--200 | 1--24 hours | Prompts change with Schema/Policy |

> **Note:** The CompileCache capacity is a **weight limit** (the sum of all SQL string lengths), not an entry count. Each SQL string's length serves as its weight; when the total weight exceeds the limit, LRU eviction is triggered.

## Cache Invalidation Strategies

### 1. TTL (Time-To-Live)

Each cache entry is created with a TTL. After expiry, it is automatically invalidated and reloaded on the next access.

```rust
// TTL of 0 means never expire
.with_compile_cache(1024, 0)
```

### 2. Manual Invalidation

Manually control caches through methods provided by `VlorQl`:

```rust
// Invalidate schema cache by version
vlorql.invalidate_schema_cache("v1.2.3");

// Invalidate compile cache by plan
let validated = vlorql.validate_only(&plan)?;
vlorql.invalidate_compile_cache(&validated).await;

// Clear all caches
vlorql.clear_all_caches();
```

### 3. LRU Eviction

When a cache reaches its capacity limit, the least recently used entries are automatically evicted. This is managed by the underlying `moka` library.

## How to Monitor Cache Hit Rates

### Via Logging

All cache operations emit `tracing::debug!` logs with HIT/MISS markers. Enable them:

```bash
# Initialize tracing-subscriber in your application
tracing_subscriber::fmt::init();

# Or via environment variable
RUST_LOG=vlorql::cache=debug cargo run
```

### Via Cache API

```rust
// Check cache size
if let Some(cache) = vlorql.compile_cache() {
    println!("Compile cache entries: {}", cache.size());
}

if let Some(cache) = vlorql.schema_cache() {
    println!("Schema cache entries: {}", cache.size());
}

if let Some(cache) = vlorql.prompt_cache() {
    println!("Prompt cache entries: {}", cache.size());
}
```

## Cache Key Design

### SchemaCacheKey

```
SchemaCacheKey {
    version: String,  // e.g. "v1.2.3"
    source: String,   // e.g. "postgres://prod-db"
}
```

### CompileCacheKey

```
CompileCacheKey {
    plan_hash: u64,       // 64-bit xxh3 hash of the normalized QueryPlan JSON
    dialect: SqlDialect,  // PostgreSQL / SQLite / MySQL
    quote_style: IdentifierQuoting,  // Identifier quoting style
}
```

### PromptCacheKey

```
PromptCacheKey {
    schema_version: String,  // Schema version number
    dialect: SqlDialect,     // Target dialect
    policy_hash: u64,        // 64-bit xxh3 hash of PolicyConfig
}
```

## Cache Interaction with the Optimizer

1. LLM generates a plan → `validate_only()` validates it
2. Optional: `QueryOptimizer` optimizes the plan
3. Check `CompileCache` → hit returns directly, miss compiles
4. After compilation, insert into `CompileCache`

## Performance Benchmarks

| Scenario | Without Cache | With Cache | Improvement |
|----------|---------------|------------|-------------|
| Compile the same plan | ~2ms | ~0.05ms | ~40× |
| Schema loading | ~1ms | ~0.01ms | ~100× |
| Prompt generation | ~0.5ms | ~0.01ms | ~50× |

Run the benchmark:

```bash
cargo bench -p vlorql-core --bench cache_bench
```

## Best Practices

1. **Enable all caches in production**: SchemaCache + CompileCache + PromptCache
2. **Set reasonable TTLs**: Schema and Prompt caches can have longer TTLs (hours), CompileCache should have a shorter TTL (30 minutes)
3. **Schema versioning**: When the schema changes, update the version number and call `invalidate_schema_cache()`
4. **CompileCache weight sizing**: Estimate based on average SQL length, each SQL ~50-500 bytes, ~1000 entries ~50-500 KB
5. **Monitor logs**: Observe cache hit rates via `RUST_LOG=vlorql::cache=debug`
6. **Concurrency safety**: All caches are backed by `moka` and are thread-safe, safe to share across `tokio::spawn`