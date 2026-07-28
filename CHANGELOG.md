# Changelog

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
