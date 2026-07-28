# Changelog

## [0.5.0] - 2026-07-28

### Added

- **CTE 类型断言方言扩展：** `render_cte_cast` 方法支持 PostgreSQL/MySQL/SQLite 三方言的显式类型转换，覆盖 Int/Float/Boolean/Decimal/Date/Timestamp/Json 等 10+ 种数据类型。
- **小模型 normalize 管道扩展：** `normalize_small_model` 新增 `from` 字符串→对象修复、LIMIT/offset 字符串→数字转换、WHERE 缺失 type 注入；覆盖 llama-3.2/qwen2.5/phi-3/deepseek-coder/gemma-2 等多种小模型。
- **`extract_json_content` 最长有效 JSON 回退：** 新增 `find_longest_valid_json` 策略，在多个 JSON 对象中优先选择最长有效解析结果。
- **`normalize_predicate` catch-all 分支：** 添加未知 predicate 类型的 `tracing::debug!` 日志和空操作处理，提升未来兼容性。
- **重试策略增强：** `format_retry_question_str` 新增 `attempt` 参数，首次仅提供错误摘要，后续逐步增加细节，减少小模型信息过载。
- **统一数据类型映射：** 新建 `common.rs` 统一 `canonical_data_type()` 和 `resolve_sql_type_alias()` 入口，消除 `expr.rs` 与 `value.rs` 间的重复维护。
- **架构文档：** 为 `parser_v2` 模块撰写完整流水线架构文档，包括数据流图、错误处理和多模型支持说明。
- **部署指南：** 中英文部署指南补充 `VlorQl::run()` 执行器使用说明。

### Fixed

- **`normalize_impl` 字段保护：** `std::mem::take` 后 `normalize_predicate` 的重建操作不再丢失非 predicate 字段（如 `data_type`）。
- **cargo doc 警告消除：** 修复 `vlorql-core` 中 3 个 unresolved link 警告（`VlorQlBuilder::build`、`default_*`、`FunctionCall`）。
- **DISTINCT + GROUP BY 严格性放宽：** 从硬错误降为 `tracing::warn!` 警告，兼容 MySQL `SELECT DISTINCT ... GROUP BY` 语义。
- **SELECT * + GROUP BY 语义检测：** 新增验证器检查，拦截 `*` 展开后可能不完全在 GROUP BY 中的场景。

### Changed

- **编译器验证：** CTE 字面量 CAST 覆盖更多数据类型（Date/Timestamp/Json/Null/Uuid），新增 MySQL/SQLite 方言 CTE 类型断言。
- **文档完善：** `parser_v2/mod.rs` 模块文档从 16 行扩展至 67 行，包含完整流水线图和多模型支持说明。

## [0.4.0] - 2026-07-28

### Added

- **执行器体系（F1）：** 新增 `DatabaseExecutor` trait，提供 `PgExecutor` 实现及 `VlorQl.run()` 入口方法，支持 PostgreSQL 查询执行。
- **Phase 4 收尾：** 完成 MySQL 与 SQLite 执行器集成，补齐多数据库后端执行能力。
- **可观测性增强（Phase 4）：** 增强 metrics 指标采集与 tracing spans 追踪，提升运行时可观测性。
- **子查询派生表（F5）：** `FromClause` 支持 Subquery 派生表（Derived Table），允许在 FROM 子句中使用子查询。
- **SQL 注入审计（F3）：** 新增 `AuditStage` SQL 注入检测审计阶段，集成至校验管道（pipeline）。
- **LLM 响应缓存（F6）：** 新增 `LlmResponseCache` 缓存层，集成至 VlorQl 外观（facade）接口。
- **类型系统扩展（F2-1）：** `DataType` 新增 `Decimal`、`Array`、`Jsonb`、`Blob` 四种数据类型，完成类型系统接入。
- **规范化增强（F2-2）：** `decimal`/`numeric`→`decimal` 别名统一，`is_canonical` 标志纳入新类型系统。
- **LLM 小模型规范化管道：** 新增面向 LLM 小模型的特定 normalize pipeline，提升小模型输出兼容性。
- **`Predicate::True`/`False` 变体：** 为 optimizer 添加常量 `Predicate` 变体，替代 `TRUE = TRUE` 模拟方式，简化 predicate 简化逻辑。
- **重试逻辑解耦：** 提取 `RetryableHttpClient` trait，消除 5 个 LLM 提供商客户端中的重试/SSE 驱动重复代码。
- **通用 `SqlxExecutor`：** 创建泛型 `SqlxExecutor<P: SqlxPool>`，统一 MySQL/SQLite 执行器实现。

### Fixed

- **JSON-schema 补齐（F2 遗留）：** 补充 `data_type` enum 中缺失的 `decimal`、`array`、`jsonb`、`blob` 类型定义。
- **小模型 normalize 多场景修复：** `order_by` 缺少 `expr` 时过滤、`op`/`right` 误嵌套在 `left` 中时提升、aggregate 简写转 `function_call`、`BETWEEN` 中 `left` → `expr` 重命名、`type:expr` 转 `comparison > 0`、递归 CTE 标志降级、CTE 内递归 normalize。
- **GROUP BY aggregate 校验：** 拒绝在 `GROUP BY` 子句中使用聚合函数。
- **`Predicate::True`/`False` 未覆盖的 match arms：** 补全 compile/fix/validate/optimize/policy 中 15+ 处遗漏的 match arms，消除编译错误。

### Changed

- **默认重试次数：** 从 2 次提升至 3 次，提升小模型容错性。
- **CLI 重构：** `LlmOverrides` 从 5 元组替换为命名结构体，提升可读性。
- **LLM 缓存优化：** 返回 `Arc<QueryPlan>` 而非克隆，减少大计划内存开销。
- **Predicate 简化器优化：** 使用 `Predicate::True`/`False` 作为常量折叠后的规范形式，减少 170+ 行冗余代码。

### Security

- **SQL 注入审计（F3）：** 新增 `AuditStage` 用于检测 SQL 注入风险，集成至校验管道，在执行前拦截恶意输入。
