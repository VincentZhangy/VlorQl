# Changelog

## [0.4.0] - 2026-07-27

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

### Fixed

- **JSON-schema 补齐（F2 遗留）：** 补充 `data_type` enum 中缺失的 `decimal`、`array`、`jsonb`、`blob` 类型定义。

### Changed

- 无

### Security

- **SQL 注入审计（F3）：** 新增 `AuditStage` 用于检测 SQL 注入风险，集成至校验管道，在执行前拦截恶意输入。
