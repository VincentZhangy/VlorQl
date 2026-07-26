# 新特性 Implementation Plan (F1–F6) — 骨架

> **状态：骨架。** 这 6 项跨多个独立子系统，**每一项应各自成为一份独立计划（独立 PR）**。本文件是拆分索引 + 每项的边界与决策点，待你确认优先级后逐一展开。
> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / superpowers:executing-plans。

**Goal:** 补齐执行层、类型系统、审计、文档、派生表与 LLM 缓存等能力缺口。

## Global Constraints

（与 `2026-07-26-correctness-fixes.md` 的 Global Constraints 相同，执行时以那份为准。）

**新特性额外约束**（源自旧执行计划 Phase 4/6）：
- 每个新特性必须有关联 integration test；安全相关（F3）须有专门 security test。
- 影响性能的改动须更新 `benches/`。
- 新增依赖必须过 `cargo deny check`（许可证白名单 + advisory）。
- F1/F2/F5 涉及 QueryPlan / 公共 API 扩展 —— 属**公共 API 变更**，须评估对 facade re-export 与现有示例的影响。

---

## 拆分索引（每项 → 独立计划）

| 项 | 特性 | 现状（已核实） | 新增/改动位置 | 规模 | 决策点 |
|----|------|----------------|----------------|------|--------|
| **F1** | `DatabaseExecutor` 统一执行层 | 缺失；用户手写 ~2900 行样板（end_to_end_pg.rs） | 新建 `vlorql-core/src/execute/`（trait + pg/mysql/sqlite）；`vlorql` 加 `run`/`execute` | 大 | 驱动依赖是否 feature-gate？先支持哪个数据库？ |
| **F2** | `DataType` 扩展 | 只有 9 变体，无 `serde(other)` fallback | `schema/types.rs` 加 Decimal/Array/Jsonb/Blob（+ `#[serde(other)] Unknown`）；同步 `compile/builder.rs`、`validate/operand.rs` match | 中 | 先加哪些类型？是否加 Unknown 兜底？ |
| **F3** | SQL 注入来源审计 | 有两层防线（quote_identifier + schema 校验），无专门审计 | 新建 `validate/audit.rs`（标识符来源 schema-derived vs LLM-derived + 一致性断言/日志） | 中 | 审计为硬失败还是仅告警？ |
| **F4** | 公共 API `# Examples` 覆盖 | `deny(missing_docs)` 已启用但 examples 稀疏（optimizer prune/pushdown/join_reorder/visitor/analyze 为 0；parser_v2 builder/mod 为 0） | 多文件补 `# Examples` doctest | 小 | 纯文档，可随时做 |
| **F5** | `FROM (subquery)` 派生表 | `FromClause` 只有 `table`+`alias`，无法表达派生表 | `schema/query_plan.rs:44` FromClause 改枚举/加变体；同步 `compile/builder.rs` FROM 生成、`validate/schema.rs` 作用域、join 的 right_table | 大 | 数据模型变更影响面广，建议独立 PR |
| **F6** | `LlmResponseCache` | 只有 Schema/Compile/Prompt 三种缓存，无"问题→计划"缓存 | 新建 `cache/llm_cache.rs`（key = 规范化问题 + schema 版本 + 模型指纹）；`vlorql/lib.rs:174 query()` LLM 调用前接入 | 中 | key 是否含模型温度/参数指纹？失效策略？ |

---

## 建议顺序

1. **F4**（文档，零风险，可先清）
2. **F2**（DataType 扩展，为 F1/F5 铺路）
3. **F6**（LLM 缓存，独立、收益直接）
4. **F3**（注入审计，安全）
5. **F1**（执行层，工作量最大）
6. **F5**（派生表，数据模型变更影响面最广，放最后）

## Execution Handoff

骨架待确认。请指定**先展开哪一项**（推荐 F4 或 F2）及其决策点答案，我再把该项展开为独立的、完整 bite-sized TDD 计划文件。
