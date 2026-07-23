# VlorQl 后续执行计划

> 版本: v0.2.0 — 2026-07-23

---

## Phase 1: 架构清理

**优先级**: P0  
**目标**: 消除模块重复，统一代码基  
**约束**:
- 所有现有测试必须全量通过 (`cargo test --workspace`)
- 不修改公共 API 签名
- golden test 用例可能因规范化字段顺序不同需要更新

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 1.1 移除 `parse/` 旧模块，统一使用 `parser_v2` | 2h | - | `crates/vlorql-llm/src/parse/` 目录移除 |
| 1.2 合并 `canonicalize.rs` 与 `parser_v2/normalize/` 中重复的数据类型处理 | 1h | 1.1 | 单一数据标准化入口 |
| 1.3 `normalize/value.rs` 与 `normalize/expr.rs` 联合重构，确保 `std::mem::take` 不丢失字段 | 3h | 1.2 | 修复 `data_type` 字段丢失问题，移除 validator 层兜底 |
| 1.4 整理 `vlorql-llm/src/lib.rs` 中的 re-export | 0.5h | 1.1 | 只暴露 parser_v2 |

---

## Phase 2: 编译器正确性

**优先级**: P0  
**目标**: 修复 PostgreSQL 执行层面的关键语法错误  
**约束**:
- PostgreSQL / MySQL / SQLite 三种方言各保持语法正确
- 参数化查询占位符 (`$N` / `?`) 必须与方言一致
- 编译后的 SQL 必须在目标数据库上可直接执行

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 2.1 `UNION ALL` + `ORDER BY` 语法修复 —— 检测 `set_operation` 存在时推迟 ORDER BY | 2h | - | `build_query()` 中条件判断 |
| 2.2 CTE 中类型断言 —— 在 `WITH RECURSIVE` 上下文中为字面量生成 `CAST` | 1h | - | `render_expression_to()` CTE 感知 |
| 2.3 `DISTINCT` + `GROUP BY` 同时存在时验证器发出警告 | 1h | - | `validate/schema.rs` 新检查 |
| 2.4 验证 `SELECT *` + `GROUP BY` 语义 | 1h | - | 扩展 `AggregationMismatch` 检查 |

---

## Phase 3: 解析器鲁棒性

**优先级**: P1  
**目标**: 提升对不同 LLM（特别是小模型）输出的容错能力  
**约束**:
- 必须兼容现有 6 个 LLM 提供商（Anthropic/DeepSeek/Zhipu/Ollama/vLLM/OpenAI）
- 小模型 (3B 以下) 的首次通过率不低于 60%
- 最大重试次数不超过 3 次

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 3.1 `extract_json_content` 增加最长有效 JSON 匹配策略 | 1h | - | 处理多个 JSON 对象的场景 |
| 3.2 `normalize_predicate` 增加对 `literal` 等非标准谓词类型的显式处理 | 1h | 1.2 | 避免 `std::mem::take` 意外删除字段 |
| 3.3 为 llama3.2/qwen2.5 等小模型添加模型特定 normalize pipeline | 2h | - | `normalize_for_model()` 扩展 |
| 3.4 重试策略分级：第一次只给摘要错误，后续逐步增加细节 | 2h | - | `format_retry_question()` 分级实现 |
| 3.5 添加模型温度调整策略 —— 第一次 0.1，重试时逐步提高 | 0.5h | 3.3 | `LlmConfig.temperature` 动态调整 |

---

## Phase 4: 可观测性与运维

**优先级**: P1  
**目标**: 生产环境可观测能力  
**约束**:
- 指标默认开启且不影响主流程性能
- OTel exporter 配置与 OpenTelemetry SDK v0.28 兼容

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 4.1 为每个流水线阶段添加 OpenTelemetry span | 1h | - | `pipeline.rs` 中各阶段的 tracing |
| 4.2 添加 LLM 调用延迟和 token 消耗指标 | 1h | - | `LlmClient` 的仪表化 |
| 4.3 编译缓存命中率统计 | 0.5h | - | `CompileCache` 指标 |
| 4.4 验证流水线各阶段的耗时和错误率指标 | 1h | - | `ValidationPipeline` 指标 |

---

## Phase 5: 文档完善

**优先级**: P2  
**目标**: API 文档覆盖率达标，中英文同步  
**约束**:
- `cargo doc --no-deps` 无警告
- 中文/英文文档要保持同步

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 5.1 `vlorql-core` 中缺少 `#[example]` 的公共方法补充文档 | 2h | - | 文档示例 |
| 5.2 为 `parser_v2` 模块撰写架构文档 | 1h | - | `parser_v2/mod.rs` 模块文档 |
| 5.3 完善 `docs/` 目录的中英文部署指南 | 1h | - | 中英文 README / 部署指南 |
| 5.4 整理 CHANGELOG 和迁移指南 | 0.5h | - | `CHANGELOG.md` |

---

## Phase 6: 未来特性

**优先级**: P2  
**目标**: 扩展能力边界  
**约束**:
- 所有新特性必须有关联的 integration test
- 安全相关功能必须有专门的 security test
- 性能基准测试（`benches/`）必须更新以反映变化

| 任务 | 估计工时 | 依赖 | 交付物 |
|------|---------|------|--------|
| 6.1 `DataType::Decimal` 类型支持和相关编译器渲染 | 2h | - | Decimal 类型 + 编译器 |
| 6.2 统一 `DatabaseExecutor` trait 和 PostgreSQL/MySQL/SQLite 实现 | 3h | - | `executor` 模块 |
| 6.3 SQL 注入审计扫描工具 | 1h | - | 审计 helper |
| 6.4 `FROM subquery` 支持（当前只有 CTE 可嵌套） | 4h | - | `FromClause` 扩展 |
| 6.5 用户自定义函数注册机制 | 2h | - | `FunctionRegistry` 扩展 |
| 6.6 LLM 响应缓存（复用相同问题的生成结果） | 2h | - | `LlmResponseCache` |

---

## 时间线总览

```text
Phase 1 ───▐█████████████████░░░░░░░░░░░░░░░░░░░│  2026-07-30
Phase 2 ───▐█████████████████████████░░░░░░░░░░░│  2026-07-30
Phase 3 ───▐█████████████████████████████████░░░│  2026-08-07
Phase 4 ───▐█████████████████████████████████░░░│  2026-08-07
Phase 5 ───▐████████████████████████████████████│  2026-08-21
Phase 6 ───▐████████████████████████████████████│  2026-08-21
```

---

## 风险登记

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|---------|
| LLM 提供商 API 变更 | 中 | 高 | 提供商的客户端隔离，统一 `LlmClient` trait |
| 小模型持续无法生成有效 Plan | 高 | 中 | 改进模型特定 prompt + normalize 兜底 |
| 缓存键冲突 | 低 | 中 | CacheKey 包含完整哈希，定期集成测试 |
| 新方言（如 MSSQL）适配困难 | 中 | 低 | `SqlCompiler` trait 设计已预留扩展点 |
