# 解析器 / 重试优化 Implementation Plan (O1–O4) — 骨架

> **状态：骨架。** 全局约束与任务边界已定；每个任务的 bite-sized TDD 步骤待你确认范围后展开。
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / superpowers:executing-plans。

**Goal:** 提升对不同 LLM（尤其小模型 <3B）输出的容错与首次通过率，降低重试时的信息过载。

**Architecture:** 三处独立优化：重试反馈分级（vlorql facade 层）、重试温度动态调整（跨 llm 层接口）、可选的 schema 驱动校验（parser_v2 validate 层）。均不改动 QueryPlan 数据模型。

**Tech Stack:** Rust (edition 2024)、既有 `LlmClient` trait、serde_json。

## Global Constraints

（与 `2026-07-26-correctness-fixes.md` 完全相同，此处引用不重复；执行时以那份的 Global Constraints 为准）

**本计划额外约束**（源自旧执行计划 Phase 3）：
- 必须兼容现有全部 LLM 提供商（Anthropic / DeepSeek / Zhipu / Ollama / vLLM / OpenAI）。
- 小模型（3B 以下）首次通过率不低于 **60%**。
- 最大重试次数不超过 **3** 次（沿用 `max_retries`）。
- O3 若需改动 `LlmClient::generate_plan` 接口，属**公共 API 变更**，须在任务中显式声明并同步所有 provider 实现与文档。

---

## File Structure（骨架）

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql/src/lib.rs`（`format_retry_question` :866 / `query` 重试循环 :215-301 / `run_stream_with_retry` :923） | 重试反馈分级 | O1、O2 |
| `crates/vlorql-llm/src/lib.rs`（`LlmConfig.temperature` :176-197）+ 各 provider | 重试温度动态调整 | O3 |
| `crates/vlorql-llm/src/parser_v2/validate/`（`validator.rs`、`semantic.rs`） | 可选：schema 驱动严格校验 | O4 |

---

## Task O1: 重试反馈截断 —— 避免小模型信息过载

**问题（已核实）：** `format_retry_question`（vlorql/lib.rs:867-872）用 `; ` 拼接**全部** validation errors 且每个 ColumnNotFound 追加 TIP，无截断。对小模型信息过载。
**方向：** 限制单次反馈的错误条数（如首屏 top-N）与 TIP 数量；保留最相关错误。
**待展开：** 失败测试（构造 >N 个 error，断言反馈被截断且含"还有 k 个错误"提示）→ 实现 → 通过 → commit。

## Task O2: 分级重试（首次摘要 → 后续加细节）

**问题（已核实）：** 重试循环（vlorql/lib.rs:215,298）每次失败都无差别调用同一 `format_retry_question`，不使用 `attempt` 调整详略。
**方向：** 给 `format_retry_question` / `format_retry_question_str` 增加 `attempt`（或 `detail_level`）参数；第 0 次只给摘要，后续逐步增加细节。`run_stream_with_retry`（:942/963/985）同步。
**依赖：** 与 O1 同区，建议在 O1 之后。
**待展开：** 失败测试（同一组 errors，attempt=0 与 attempt=2 反馈详略不同）→ 实现 → 通过 → commit。

## Task O3: 重试温度动态调整

**问题（已核实）：** `temperature` 为静态配置（vlorql-llm/lib.rs:176-197，默认 0.0），重试循环不调整。
**方向：** 首次低温（0.1），重试逐步提高。`LlmClient::generate_plan(&question, &system_prompt)` 当前不带温度参数 —— 需二选一：(a) 扩展 trait 方法签名（公共 API 变更，同步全部 provider）；(b) 重试时用更高温度重建 client。**决策点：需你确认 (a) 还是 (b)。**
**待展开：** 依决策定测试与实现 → commit。

## Task O4（可选）: JSON Schema 驱动严格校验

**问题（已核实）：** 当前 canonicalization + validate 全是硬编码规则（value.rs:9-41、validator.rs、semantic.rs），无 schema 驱动校验。
**方向：** 用 `schemars` 从 `QueryPlan` 生成 JSON Schema，在 build 前对规范化后的 value 做 schema 校验，捕捉硬编码规则未覆盖的边缘情况。**范围较大、可能引入新依赖（jsonschema）需过 `cargo deny`。建议作为独立 PR，或从本计划剔除。需你确认是否纳入。**

## Execution Handoff

骨架待确认。确认范围（尤其 O3 决策、O4 是否纳入）后，我会把每个任务展开为 bite-sized TDD 步骤（完整测试代码 + 实现 + 命令 + commit）。
