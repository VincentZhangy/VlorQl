# F6: LlmResponseCache — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

为 VlorQl 增加"问题 → QueryPlan"的 LLM 响应缓存（LlmResponseCache），避免对同一问题的重复 LLM 调用，降低延迟和 API 费用。

## Architecture

在 `vlorql-core/src/cache/` 下新建 `llm_cache.rs`，复用项目已有的 `Cache` trait 风格，后端使用 `moka::future::Cache`（与 `CompileCache`/`SchemaCache` 一致）。在 `vlorql/src/lib.rs` 的 `query()` 方法中，在 `client.generate_plan()` 调用前插入缓存查找逻辑。

**不涉及：** 修改 `query()` 公共 API 签名、修改数据模型、新增第三方依赖。

---

## Design

### Cache Key

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LlmCacheKey {
    pub normalized_question: String,
    pub schema_version: String,
    pub model_fingerprint: String,
}
```

选择理由：
- **`normalized_question`** — 规范化（去空白、小写）后的用户问题，确保等价问题命中同一缓存
- **`schema_version`** — 来自 `schema.version`，schema 变更时旧 plan 不可用
- **`model_fingerprint`** — `"{provider}:{model_name}"`，不同模型返回结果可能不同

**不纳入 key：** 温度/参数（`temperature`、`max_tokens` 等）。理由：同一问题应返回同一 plan，温度只影响 LLM 的采样路径，不影响语义正确的 plan。用户若需要不同参数获得不同结果，应手动 `invalidate`。

### Cache Value

```rust
type LlmCacheValue = Arc<QueryPlan>;
```

`Arc` 零成本共享，避免 clone 开销。

### Data Structure

```rust
pub struct LlmResponseCache {
    inner: moka::future::Cache<LlmCacheKey, LlmCacheValue>,
}
```

后端选项：`moka::future::Cache`，与 `CompileCache`/`SchemaCache` 一致。特性：
- 并发安全（内部 sharded）
- LRU 淘汰
- TTL 支持

### Default Configuration

| Parameter | Default | Configurable |
|-----------|---------|--------------|
| Max entries | 1000 | Yes |
| TTL | 1 hour | Yes |

### API

```rust
impl LlmResponseCache {
    pub fn new(max_entries: u64, ttl_seconds: u64) -> Self;
    pub async fn get(&self, key: &LlmCacheKey) -> Option<Arc<QueryPlan>>;
    pub async fn insert(&self, key: LlmCacheKey, plan: Arc<QueryPlan>);
    pub fn invalidate_question(&self, question: &str);
    pub fn invalidate_schema_version(&self, version: &str);
    pub fn clear(&self);
    pub fn size(&self) -> u64;
}
```

### Integration Point

在 `vlorql/src/lib.rs` 的 `query()` 方法，LLM 调用前插入：

```
query(question, schema, ...) {
    key = LlmCacheKey::new(normalize(question), schema.version, model_fingerprint)
    if let Some(cached) = self.llm_cache.get(&key).await {
        return Ok(cached)
    }
    plan = generate_plan(question, ...).await
    self.llm_cache.insert(key, Arc::new(plan.clone())).await
    Ok(plan)
}
```

- `VlorQlEngine` 结构体新增 `llm_cache: LlmResponseCache` 字段，默认 `new(1000, 3600)`
- 不改变 `query()` 的公共签名

### Cache Invalidation 策略

| 事件 | 操作 |
|------|------|
| TTL 到期 | 自动淘汰（moka 内置） |
| 用户调用 `clear_cache` | 调用 `cache.clear()` |
| Schema 更新 | 调用 `cache.invalidate_schema_version(old_version)` |
| 显式失效指定问题 | 调用 `cache.invalidate_question(question)` |

---

## Testing

| 测试 | 描述 |
|------|------|
| `cache_hit_returns_cached_plan` | 相同 key 第二次调用返回缓存值 |
| `different_key_misses` | 问题/schema/模型不同时不命中 |
| `ttl_expiry_causes_miss` | TTL 过期后重新生成 |
| `invalidate_question_removes_entry` | 指定问题失效 |
| `invalidate_schema_version_removes_entries` | 版本失效 |
| `clear_removes_all_entries` | 全部清空 |
| `concurrent_access_is_safe` | 并发读写安全 |

---

## Non-goals

- 缓存持久化（进程重启后缓存丢失，符合预期）
- 分布式缓存（保持简单，后续可按需升级）
- `invalidate` 对所有匹配 key 做模式匹配（只有精确 key 失效或按版本前缀失效）
