# VlorQl 缓存系统

## 概述

VlorQl 内置三层可插拔缓存，用于减少重复计算和 I/O 开销：

| 缓存 | 作用 | 键 |
|------|------|-----|
| **SchemaCache** | 缓存 `SchemaSnapshot`，避免重复解析或拉取 | `SchemaCacheKey` (version + source) |
| **CompileCache** | 缓存 `ValidatedPlan → CompiledQuery`，避免重复编译 | `CompileCacheKey` (plan_hash + dialect + quote_style) |
| **PromptCache** | 缓存系统提示词，避免重复生成 | `PromptCacheKey` (schema_version + dialect + policy_hash) |

## 如何配置

### 通过 VlorQlBuilder 启用

```rust
use vlorql::VlorQl;

let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect(dialect)
    .with_policy(policy)
    .with_llm_client(llm_client)
    // 启用所有缓存
    .with_schema_cache(10, 3600)        // 10 条条目，TTL 1 小时
    .with_compile_cache(100 * 1024, 1800)  // 100 KB 权重上限，TTL 30 分钟
    .with_prompt_cache(50, 7200)        // 50 条条目，TTL 2 小时
    .build()?;
```

### 仅启用部分缓存

每个缓存都是可选的，只调用需要的方法即可：

```rust
// 只启用编译缓存
let vlorql = VlorQl::builder()
    .with_schema(schema)
    .with_dialect(dialect)
    .with_policy(policy)
    .with_llm_client(llm_client)
    .with_compile_cache(1024, 60)
    .build()?;
```

## 缓存大小与 TTL 建议

| 缓存 | 建议容量 | 建议 TTL | 说明 |
|------|----------|----------|------|
| SchemaCache | 10-100 | 1-24 小时 | Schema 极少变更 |
| CompileCache | 1000-10000 (权重) | 30 分钟 - 1 小时 | 缓存已编译的 SQL |
| PromptCache | 50-200 | 1-24 小时 | 提示词随 Schema/Policy 变化 |

> **注意**：CompileCache 的容量是**权重上限**（所有 SQL 字符串长度之和），而非条目数。每个 SQL 字符串的长度作为权重，当总权重超过上限时触发 LRU 淘汰。

## 缓存失效策略

### 1. TTL（时间过期）

每个缓存条目在创建时附带 TTL。到期后自动失效，下次访问时重新加载。

```rust
// TTL 为 0 表示永不过期
.with_compile_cache(1024, 0)
```

### 2. 手动失效

通过 `VlorQl` 提供的方法手动控制缓存：

```rust
// 按版本失效 Schema 缓存
vlorql.invalidate_schema_cache("v1.2.3");

// 按计划失效编译缓存
let validated = vlorql.validate_only(&plan)?;
vlorql.invalidate_compile_cache(&validated).await;

// 清空所有缓存
vlorql.clear_all_caches();
```

### 3. LRU 淘汰

当缓存达到容量上限时，最近最少使用的条目会被自动淘汰。这由底层的 `moka` 库自动管理。

## 如何监控缓存命中率

### 通过日志

所有缓存操作都会输出 `tracing::debug!` 日志，包含 HIT/MISS 标记。启用方式：

```bash
# 在应用中初始化 tracing-subscriber
tracing_subscriber::fmt::init();

# 或通过环境变量
RUST_LOG=vlorql::cache=debug cargo run
```

### 通过缓存 API

```rust
// 检查缓存大小
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

## 缓存键设计

### SchemaCacheKey

```
SchemaCacheKey {
    version: String,  // 如 "v1.2.3"
    source: String,   // 如 "postgres://prod-db"
}
```

### CompileCacheKey

```
CompileCacheKey {
    plan_hash: u64,       // QueryPlan 规范化 JSON 的 64 位 xxh3 哈希
    dialect: SqlDialect,  // PostgreSQL / SQLite / MySQL
    quote_style: IdentifierQuoting,  // 标识符引用风格
}
```

### PromptCacheKey

```
PromptCacheKey {
    schema_version: String,  // Schema 版本号
    dialect: SqlDialect,     // 目标方言
    policy_hash: u64,        // PolicyConfig 的 64 位 xxh3 哈希
}
```

## 缓存与优化器的交互

1. LLM 生成计划 → `validate_only()` 验证
2. 可选：`QueryOptimizer` 优化计划
3. 检查 `CompileCache` → 命中则直接返回，未命中则编译
4. 编译后插入 `CompileCache`

## 性能基准

| 场景 | 无缓存 | 有缓存 | 提升 |
|------|--------|--------|------|
| 编译相同计划 | ~2ms | ~0.05ms | ~40× |
| Schema 加载 | ~1ms | ~0.01ms | ~100× |
| 提示词生成 | ~0.5ms | ~0.01ms | ~50× |

运行基准测试：

```bash
cargo bench -p vlorql-core --bench cache_bench
```

## 最佳实践

1. **生产环境启用所有缓存**：SchemaCache + CompileCache + PromptCache
2. **设置合理的 TTL**：Schema 和 Prompt 缓存可以设置较长的 TTL（小时级），CompileCache 设置较短（30 分钟）
3. **Schema 版本控制**：当 Schema 变更时，更新版本号并调用 `invalidate_schema_cache()`
4. **CompileCache 权重设置**：根据平均 SQL 长度估算，每个 SQL 约 50-500 字节，1000 条约占 50-500 KB
5. **监控日志**：通过 `RUST_LOG=vlorql::cache=debug` 观察缓存命中率
6. **并发安全**：所有缓存底层基于 `moka`，是线程安全的，可在 `tokio::spawn` 中安全共享