# VlorQl 用户指南目录

## 概述

- [项目介绍](../../README.md)
- [架构设计](./DESIGN.md)

## 模块

- [Schema 和查询计划](../../crates/vlorql-core/src/schema/mod.rs)
- [验证](../../crates/vlorql-core/src/validate/mod.rs)
- [编译](../../crates/vlorql-core/src/compile/mod.rs)
- [策略引擎](../../crates/vlorql-core/src/policy/mod.rs)
- [查询优化器](../../crates/vlorql-core/src/optimizer/mod.rs)
- [统计信息](../../crates/vlorql-core/src/statistics/mod.rs)
- [缓存](../../crates/vlorql-core/src/cache/mod.rs)
- [提示词构建](../../crates/vlorql-core/src/prompt/mod.rs)
- [LLM 客户端](../../crates/vlorql-llm/src/lib.rs)
- [Facade API](../../crates/vlorql/src/lib.rs)
- [命令行接口](../../crates/vlorql-cli/src/main.rs)

## 指南

- [用户指南](./guide.md)
- [部署指南](./deployment.md)
- [优化指南](./optimization.md)
- [缓存指南](./caching.md)
- [可观测性指南](./observability.md)

## 性能

- [基准测试](./BENCHMARKS.md)

## API 参考

- 通过 `cargo doc --workspace --no-deps --open` 生成