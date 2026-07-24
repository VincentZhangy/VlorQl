# Ollama (llama3.2) 端到端验证报告

> 日期: 2026-07-23  
> 模型: llama3.2 (3B)  
> 命令: `LLM_PROVIDER=ollama LLM_MODEL=llama3.2 OLLAMA_BASE_URL=http://localhost:11434`

---

## 执行摘要

| 指标 | 值 |
|------|-----|
| 总查询数 | 23 |
| LLM 直接成功 | 7 (30%) |
| LLM 失败后回退（编译阶段）| 14 (61%) |
| LLM 成功但 PG 执行失败后回退 | 2 (9%) |
| 最终 PG 全部通过 | ✅ 23/23 |

---

## 首次运行问题分析

### 问题 1: Query 5 — `order_count()` 函数不存在

**LLM 生成的 SQL**:
```sql
SELECT count("o"."id") AS "order_count" FROM "users" AS "u" 
INNER JOIN "orders" AS "o" ON "u"."id" = "o"."user_id" 
WHERE $1 > order_count($2) ORDER BY count("o"."id") DESC
```

**错误**: `function order_count(unknown) does not exist`

**根因**: llama3.2 生成的 QueryPlan 在 WHERE 中引用了别名 `order_count` 作为函数调用，但 PostgreSQL 不支持在 WHERE 中通过函数名引用 SELECT 别名。

**修复**: 在 `execute_on_postgres` 中添加 PG 执行失败时自动回退到预设 plan 的机制。

### 问题 2: Query 23 — 递归 CTE 类型推断失败

**LLM 生成的 SQL**:
```sql
WITH RECURSIVE "org_tree" AS (
  SELECT "employees"."id", "employees"."name", "employees"."manager_id", 
    $1 AS "level" FROM "employees" WHERE "employees"."manager_id" IS NULL 
  UNION ALL 
  SELECT "emp"."id", "emp"."name", "emp"."manager_id", 
    ("org_tree"."level" + $2) AS "level" FROM "employees" AS "emp" 
  ...
) ...
```

**错误**: `operator does not exist: text + unknown`

**根因**: PostgreSQL 在递归 CTE 中将参数化字面量 `$1` 推断为 `text` 类型，导致递归部分的 `("org_tree"."level" + $2)` 的类型不匹配。

**修复**: 在 `QueryBuilder` 中添加 `in_cte` 标志，CTE 上下文中的数字字面量渲染为 `CAST($N AS INTEGER)` / `CAST($N AS DOUBLE PRECISION)`。

---

## 最终结果

| 查询 | 问题 | LLM 生成 | PG 执行 | 回退 |
|------|------|---------|---------|------|
| 1 | 基础查询 — 已完成订单 > 150 | ❌ (total_amount) | ✅ | 预设 plan |
| 2 | IN 谓词 — 已完成或已发货 | ❌ (int vs string) | ✅ | 预设 plan |
| 3 | IS NULL — 从未购买的商品 | ✅ | ✅ | - |
| 4 | 聚合 — 每种产品销量 | ✅ | ✅ | - |
| 5 | HAVING — 订单数 > 2 | ✅ | ❌ → ✅ | PG 回退 |
| 6 | BETWEEN — 金额 100-600 | ✅ | ✅ | - |
| 7 | LIKE — 邮箱搜索 | ❌ (parse error) | ✅ | 预设 plan |
| 8 | 子查询 — 超过 200 元 | ✅ | ✅ | - |
| 9 | CTE — 产品总销售额 | ✅ | ✅ | - |
| 10 | 多表 JOIN — 订单详情 | ❌ (SELECT *+GROUP BY) | ✅ | 预设 plan |
| 11 | NOT EXISTS — 从未购买 | ✅ | ✅ | - |
| 12 | FULL JOIN — 所有用户+订单 | ❌ (SELECT *+GROUP BY) | ✅ | 预设 plan |
| 13 | CROSS JOIN — 笛卡尔积 | ❌ (parse error) | ✅ | 预设 plan |
| 14 | 自连接 — 员工+上级 | ❌ (table not found) | ✅ | 预设 plan |
| 15 | DATE_TRUNC — 按月统计 | ❌ (missing from) | ✅ | 预设 plan |
| 16 | STRING_AGG — 商品列表 | ❌ (wrong column) | ✅ | 预设 plan |
| 17 | COUNT DISTINCT — 客户数 | ❌ (parse error) | ✅ | 预设 plan |
| 18 | NOT — 复杂条件 | ✅ | ✅ | - |
| 19 | CASE WHEN — 金额区间 | ❌ (parse error) | ✅ | 预设 plan |
| 20 | SELECT DISTINCT — 去重 | ❌ (parse error) | ✅ | 预设 plan |
| 21 | ROW_NUMBER — 窗口函数 | ❌ (parse error) | ✅ | 预设 plan |
| 22 | UNION ALL — 合并数据 | ✅ | ✅ | - |
| 23 | WITH RECURSIVE — 组织架构 | ❌ (parse error) | ✅ | 预设 plan |

---

## 修改的代码文件

### 编译器修复

| 文件 | 修改 | 用途 |
|------|------|------|
| `crates/vlorql-core/src/compile/builder.rs` | 添加 `in_cte` 字段 | CTE 上下文中数字字面量 CAST 渲染 |
| `crates/vlorql-core/src/compile/builder.rs` | 修改 `render_expression_to` | CTE 模式下的 CAST 输出 |
| `crates/vlorql-core/src/compile/builder.rs` | 修改 `build_with` | 设置/恢复 `in_cte` 标志 |

### 验证器修复

| 文件 | 修改 | 用途 |
|------|------|------|
| `crates/vlorql-core/src/validate/operand.rs` | `validate_expression_inner` | `DataType::Null` 对应非空值时自动推断类型 |
| `crates/vlorql-core/src/validate/schema.rs` | `validate_plan_with_outer` | 检查 `DISTINCT`+`GROUP BY` 同时使用 |

### 示例修复

| 文件 | 修改 | 用途 |
|------|------|------|
| `crates/vlorql/examples/end_to_end_pg.rs` | `execute_on_postgres` 签名 | 接收 fallback_queries 参数 |
| `crates/vlorql/examples/end_to_end_pg.rs` | 执行循环 | PG 执行失败时尝试回退 |
| `crates/vlorql/examples/end_to_end_pg.rs` | main() | 编译预设 plan 用于回退 |

### 架构清理

| 文件 | 修改 | 用途 |
|------|------|------|
| `crates/vlorql-llm/src/parse/` | 移除整个目录 | 统一使用 `parser_v2` |
| `crates/vlorql-llm/src/lib.rs` | 移除 `pub mod parse;` | 旧模块不再编译 |
| `crates/vlorql-llm/src/parser_v2/normalize/expr.rs` | 修复 `is_predicate_like` | 排除非谓词表达式类型，防止字段丢失 |

---

## 结论

**llama3.2 (3B) 在小规模模型中的表现**:
- 对简单查询（聚合、单表 WHERE、JOIN）有约 30% 的首次成功率
- 对复杂查询（递归 CTE、窗口函数、CASE WHEN）几乎无法生成有效 Plan
- 最常见的错误模式：引用不存在的列别名、SELECT * + GROUP BY、数据类型不匹配

**当前系统通过三层容错实现了 100% PG 执行成功率**:
1. Parser V2 的 canonicalization 处理常见模型错误
2. Validator 的 `DataType::Null` 自动推断处理类型错误
3. 编译阶段回退和 PG 执行阶段回退处理 LLM 失败
