# Schema-Aware Fixer 设计文档

> 日期: 2026-07-24
> 状态: 已批准
> 策略: 混合方案 C — 轻量 Schema-aware fixer + Prompt 简化

---

## 1. 背景

移除 `auto_join_missing_tables` 后，模型能力不足导致 4 类典型错误：

| 错误类型 | 示例 | 严重度 |
|---|---|---|
| 遗漏 JOIN | 引用 `users.name` 但未 JOIN users | 高 |
| 聚集嵌套 | `HAVING SUM(SUM(total))` | 高 |
| GROUP BY 错误 | `GROUP BY literal null` 或漏列 | 高 |
| 不必要的聚合 | 对 `orders.total` 用 SUM() 导致强行 GROUP BY | 中 |

其中前三条有明确的 AST 修复规则，适合用代码保底。第四条有一定语义歧义，留给 Prompt 引导 + 重试。

---

## 2. 架构

### 2.1 Pipeline 位置

```
LLM output ──► parse ──► build ──► fix_plan() ──► schema_fix() ──► validate ──► compile
                              ▲           ▲              ▲
                              │           │              │
                         normalizer    fixer.rs    新: schema-aware
                         (JSON ops)   (无 schema)   (有 SchemaSnapshot)
```

`schema_fix` 在 `ValidationPipeline` 中执行，在 compile 之前、原有的 `fix_plan` 之后。

### 2.2 新增模块

```
crates/vlorql-core/src/fix/
├── mod.rs            # pub fn schema_aware_fix(plan, schema) → bool
├── joins.rs          # Rule 1: 修复遗漏 JOIN
├── aggregates.rs     # Rule 2: 去重嵌套聚合
└── group_by.rs       # Rule 3: 修正 GROUP BY
```

### 2.3 接口签名

```rust
/// 运行 Schema-aware 修复，返回 true 表示做了修改。
/// 在 validate 之前调用。
pub fn schema_aware_fix(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool;
```

---

## 3. 修复规则详细设计

### 3.1 Rule 1: 修复遗漏 JOIN

**问题：** SELECT/WHERE 中引用 `users.name`，但 `users` 不在 FROM/JOINS 中。

**原理：** 遍历 `Plan` 的所有 `ColumnReference`，收集有 `table` 限定符的引用。对每个不在 query scope 中的表，查 Schema 的 FK 关系找到正确 JOIN 路径。

**搜索策略：**
1. 遍历 Schema 中所有表，查找 FK 指向目标表的列（反向 FK 索引）
2. 如果是 `orders.user_id → users.id`，且 `orders` 已在 FROM 中
   → 生成 `INNER JOIN users ON orders.user_id = users.id`
3. 如果是 `order_items.order_id → orders.id`，且 `orders` 已在 FROM 中
   → 生成 `INNER JOIN order_items ON orders.id = order_items.order_id`

**不是猜测**——而是查 Schema 的实际 FK 定义。

**实现：**
```rust
fn fix_missing_joins(plan: &mut QueryPlan, schema: &SchemaSnapshot) -> bool;
```

### 3.2 Rule 2: 去重嵌套聚合

**问题：** `SUM(SUM(x))`、`COUNT(COUNT(x))`

**原理：** 递归遍历所有 Expression 位置（SELECT、WHERE、HAVING、ORDER BY），检测 `FunctionCall(func, args)` 其中 `args[0]` 也是同名的 `FunctionCall` → 去掉外层。

**匹配模式：**
- `sum(sum(x))` → `sum(x)`
- `count(count(x))` → `count(x)`
- `avg(avg(x))` → `avg(x)`
- 仅在函数名完全一致时去重

**实现：**
```rust
fn deduplicate_nested_aggregates(expr: &mut Expression) -> bool;
```

### 3.3 Rule 3: 修正 GROUP BY

**问题：** GROUP BY 内容错误（literal null、漏掉非聚合列）

**规则：**
1. 移除 GROUP BY 中所有 `Literal` 类型的条目（模型输出的垃圾）
2. 收集 SELECT 中所有非聚合的 `ColumnReference`（未被 aggregate 包裹）
3. 如果 GROUP BY 缺失了这些列 → 追加

**判定"非聚合列"：**
- 一个 ColumnReference 如果不在任何 `FunctionCall(sum/count/avg/min/max)` 的参数树中 → 非聚合列
- 必须在 GROUP BY 中出现

**实现：**
```rust
fn fix_group_by(plan: &mut QueryPlan) -> bool;
```

---

## 4. Prompt 简化

修改 `crates/vlorql-core/src/prompt/builder.rs`:

### 4.1 `push_type_guidance` 压缩

- 将 `Common Mistakes — WRONG vs RIGHT` 从当前约 50 行精简到 ~15 行
- 保留最关键的 3 条反模式（聚集嵌套、ORDER BY 别名、WHERE vs HAVING）
- 移除冗余的类型/谓词定义列表（模型已能生成正确 JSON）

### 4.2 新增反模式警告

```
ANTI-PATTERNS — NEVER write these:
  WRONG: {"type":"function_call","name":"sum","args":[{"type":"function_call","name":"sum",...}]}
  RIGHT: {"type":"function_call","name":"sum","args":[{"type":"column_ref",...}]}
  (Never nest the same aggregate function inside itself.)
```

### 4.3 新增 JOIN 引导

在 `push_schema_description` 末尾追加：
```
Remember: referencing `table.column` in SELECT without joining `table` first is invalid.
```

---

## 5. 文件变更清单

| 文件 | 变更 |
|---|---|
| `crates/vlorql-core/src/fix/mod.rs` (新) | `schema_aware_fix` 入口 |
| `crates/vlorql-core/src/fix/joins.rs` (新) | Rule 1 实现 |
| `crates/vlorql-core/src/fix/aggregates.rs` (新) | Rule 2 实现 |
| `crates/vlorql-core/src/fix/group_by.rs` (新) | Rule 3 实现 |
| `crates/vlorql-core/src/validate/pipeline.rs` | 在 `validate` 前插入 `schema_aware_fix` 调用 |
| `crates/vlorql-core/src/prompt/builder.rs` | 简化 `push_type_guidance` + 反模式/JOIN 引导 |

---

## 6. 测试计划

### 单元测试

- `fix/joins.rs`:
  - 缺失 users 时自动添加 `orders.user_id → users.id`
  - 已存在正确 JOIN 时不重复添加
  - 多个缺失表时全部添加
- `fix/aggregates.rs`:
  - `SUM(SUM(x))` → `SUM(x)`
  - `SUM(COUNT(x))` 保持不变（不同函数不合并）
  - 无嵌套时不变
- `fix/group_by.rs`:
  - SELECT 有 `orders.id, users.name` 时自动加入 GROUP BY
  - GROUP BY 中包含 `Literal null` 时移除
  - 非聚合列已全部在 GROUP BY 中时不变

### 集成测试

- 构造包含所有 4 种错误的 output → 验证 pipeline 输出正确 SQL
- 验证 `schema_aware_fix` 与 `fix_plan` 串联不冲突
