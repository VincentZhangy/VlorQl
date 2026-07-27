# Phase 4: 可观测性增强 — Design Spec

> **Date:** 2026-07-27
> **Branch:** `feat/0.4.0`
> **Status:** Approved

## Goal

补齐 VlorQl 可观测性能力：接入 LLM 时长指标、Schema 缓存指标、LLM Token 指标、补全 Tracing spans。

---

## A: 接入 LLM 时长指标

**现状：** `VlorqMetrics::llm_duration_histogram` 已定义但在 `vlorql/src/lib.rs` 的 LLM 调用处未记录。

**改动：** 在 `query()` 方法的 LLM 调用 `client.generate_plan()` 前后记录耗时：

```rust
let llm_start = std::time::Instant::now();
let plan = client.generate_plan(...).await;
if let Some(ref m) = self.metrics {
    m.llm_duration_histogram.record(llm_start.elapsed().as_secs_f64(), &[]);
}
```

## B: Schema 缓存指标

**现状：** 只有 CompileCache 有 hit/miss 指标，SchemaCache 没有。

**改动：** 在 `VlorqMetrics` 中新增：

```rust
pub struct VlorqMetrics {
    // ... existing ...
    pub schema_cache_hits: Counter<u64>,
    pub schema_cache_misses: Counter<u64>,
}
```

在 SchemaCache 的 `get_or_insert_with` 中记录 hit/miss。

## C: LLM Token 消耗指标

**现状：** 无 Token 消耗追踪。

**改动：** 新增：

```rust
pub llm_prompt_tokens: Counter<u64>,
pub llm_completion_tokens: Counter<u64>,
```

从 LLM 响应中提取 `Usage` 信息并记录（各个 provider 的 response 中 token 计数字段不同，需在 provider 层提取）。

## D: Tracing Spans

**现状：** `query()` 已有 `tracing::info_span!("vlorql.query")`。

**改动：** 在 pipeline 各阶段添加嵌套 span：

```rust
// validate/schema
let _span = tracing::info_span!("vlorql.validate.schema").entered();

// validate/operand
let _span = tracing::info_span!("vlorql.validate.operand").entered();

// compile
let _span = tracing::info_span!("vlorql.compile").entered();
```
