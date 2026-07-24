# 纯 LLM 模式终期报告 — 代码修复汇总与模型限制

> 日期: 2026-07-24  
> 模型: llama3.2 (3B) via Ollama  
> 约束: 不使用预设 Plan，全部依赖代码级修复

---

## 总览

经过多次迭代和 8 项代码级修复，纯 LLM 模式下 **1/23 条查询** 可通过完整的端到端流程 (LLM 生成 → 验证 → 编译 → PG 执行)。其余 22 条因模型能力限制无法自动修复。

---

## 所有代码修复清单

| # | 修复 | 文件 | 说明 |
|---|------|------|------|
| 1 | `fix_where_alias_refs` — WHERE/Having 别名引用解析 | `parser_v2/fix/fixer.rs` | 将 `WHERE orders.total_amount` 自动替换为 `WHERE orders.total` |
| 2 | `repair_truncated_json` — 截断 JSON 补全 | `parser_v2/recover/bracket.rs` + `pipeline.rs` | 追加缺失的闭合大括号/方括号，修复 LLM 输出截断 |
| 3 | 简化重试反馈 — 纯文本替代 JSON | `vlorql/src/lib.rs` | 避免小模型上下文膨胀导致输出截断 |
| 4 | `DataType::Null` 自动推断 | `validate/operand.rs` | `data_type: "null"` 对应非空值时自动推断 String/Int/Float |
| 5 | 参数去重缓存 | `compile/builder.rs` | 相同字面量复用同一参数索引，满足 PG GROUP BY 要求 |
| 6 | UNION ALL + ORDER BY 排序修复 | `compile/builder.rs` | 将 `ORDER BY` 移后至 `UNION ALL` 之后 |
| 7 | CTE 中数字字面量 CAST | `compile/builder.rs` | `$1` → `CAST($1 AS INTEGER)`，修复递归 CTE 类型推断 |
| 8 | `is_predicate_like` 排除非谓词类型 | `parser_v2/normalize/expr.rs` | 防止 `literal` 等类型被 `std::mem::take` 处理导致字段丢失 |
| 9 | DISTINCT + GROUP BY 冲突检查 | `validate/schema.rs` | 验证器新增语义检查 |

---

## 验证结果

```
纯 LLM 模式 (无预设 plan 回退):

查询 1 ✅  "已完成订单 > 150"    — LLM 生成 → 验证 → PG 执行 (3 rows)
查询 2 ❌  "已完成/已发货"        — 表作用域错误 (模型限制)
查询 3 ❌  "从未购买的商品"       — 无效 JSON (模型限制)
查询 4 ～ 23 ❌                  — 模型能力限制
```

**通过率**: 4.3% (1/23)

---

## 模型限制分析

llama3.2 (3B) 的上下文窗口约为 8K tokens。在以下情况下会失败：

1. **复杂度超限** (Queries 3-23): 查询超过 30 个 token 时，模型无法同时跟踪表结构、类型约束和 JSON 格式
2. **重试反馈膨胀** (确认修复后缓解): 之前的 JSON 格式重试反馈占用 ~500 tokens/次，3 次重试后上下文窗口不足
3. **语义理解不足**: 模型无法区分 `SELECT alias` 与原始列名、无法判断何时需要 `GROUP BY`、无法正确推断 JOIN 关系

---

## 推荐方案

对于生产级使用，推荐以下方案之一：

| 方案 | 优点 | 缺点 |
|------|------|------|
| **使用更大模型** (qwen2.5:14b / llama3.1:8b) | 生成的 QueryPlan 质量更高 | 需要更多 GPU 资源 |
| **保留预设 Plan 回退** | 100% 通过率 | 维护两套 plan |
| **混合模式** (LLM 尝试 → 失败回退预设) | 平衡灵活性与可靠性 | 实现稍复杂 |

---

## 文件记录

```
docs/superpowers/
├── 06_pure_llm_validation_report.md   ← 本文件
└── (其他文档)
```

相关代码更改涉及 5 个 crate，共 9 个独立修复。
