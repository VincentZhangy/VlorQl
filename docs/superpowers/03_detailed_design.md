# VlorQl 详细设计文档

## 1. 系统定位

**Natural Language → Parameterized SQL — AI-Native Query Engine**

将自然语言通过 LLM + 规则引擎转化为类型安全、方言感知的 SQL。

---

## 2. 核心数据模型

```rust
// crates/vlorql-core/src/schema/query_plan.rs

pub struct QueryPlan {
    pub select: Vec<Projection>,         // SELECT 列表
    pub from: FromClause,                // 主表
    pub r#where: Option<Predicate>,      // WHERE 条件
    pub joins: Option<Vec<JoinClause>>,  // JOIN 子句
    pub group_by: Option<Vec<Expression>>,// GROUP BY
    pub having: Option<Predicate>,       // HAVING
    pub order_by: Option<Vec<OrderByTerm>>,// ORDER BY
    pub limit: Option<u64>,              // LIMIT
    pub offset: Option<u64>,             // OFFSET
    pub ctes: Option<Vec<CommonTableExpression>>, // WITH 查询
    pub set_operation: Option<SetOperationClause>,// UNION/INTERSECT/EXCEPT
    pub distinct: bool,                  // SELECT DISTINCT
    pub distinct_on: Option<Vec<Expression>>,     // DISTINCT ON (PostgreSQL)
}

pub enum Projection {
    Column { table: Option<String>, column: String, alias: Option<String> },
    Expr { expression: Expression, alias: Option<String> },
    Star { table: Option<String> },
}

pub enum Expression {
    Literal { value: serde_json::Value, data_type: DataType },
    ColumnRef { table: Option<String>, column: String },
    FunctionCall { name: String, args: Vec<Expression>, distinct: bool },
    BinaryOp { left: Box<Expression>, op: BinaryOperator, right: Box<Expression> },
    Star,
    SubQuery { query: Box<QueryPlan> },
    Case { operand: Option<Box<Expression>>, when_thens: Vec<WhenThen>, else_result: Option<Box<Expression>> },
    WindowFunction { name: String, args: Vec<Expression>, distinct: bool, over: WindowSpec },
}

pub enum Predicate {
    Comparison { left: Expression, op: ComparisonOperator, right: Expression },
    And { left: Box<Predicate>, right: Box<Predicate> },
    Or { left: Box<Predicate>, right: Box<Predicate> },
    Not { child: Box<Predicate> },
    Between { expr: Expression, low: Expression, high: Expression },
    In { expr: Expression, target: InTarget },
    Like { expr: Expression, pattern: String },
    IsNull { expr: Expression },
    Exists { query: Box<QueryPlan> },
}
```

### DataType 类型系统

```rust
pub enum DataType {
    Int,        // 64-bit signed integer
    Float,      // IEEE-754 double-precision float
    String,     // Variable-length UTF-8 text
    Boolean,    // true / false
    Date,       // Calendar date without time zone
    Timestamp,  // Timestamp with microsecond precision
    Json,       // Untyped JSON value
    Null,       // SQL NULL of indeterminate type
    Uuid,       // Universally unique identifier
}
```

---

## 3. 流水线架构 (Parser V2)

```text
LLM Raw Text
  │
  ▼
[Stage 1: recover]
  提取 JSON (去 markdown fence / 找最外层 {} / 修复截断)
  │
  ▼
[Stage 2: normalize]
  11 个子阶段，操作 serde_json::Value:
  ├ aliases   — 字段名同义词 (filter→where, projection→select)
  ├ array     — 数组/单值统一 (select 单对象→[对象])
  ├ select    — SELECT 结构 (string→对象, 注入 type)
  ├ table     — FROM 结构 (string→对象)
  ├ where_    — 谓词结构 (array→对象, 提取顶层字段)
  ├ join      — JOIN 结构 (注入缺失的 on)
  ├ query     — 顶层字段清理 (移除未知字段)
  ├ operators — 操作符标准化 (=→eq, !=→neq)
  ├ value     — 数据类型标准化 (integer→int, varchar→string)
  ├ expr      — 表达式标签注入 (column→column_ref)
  └ order     — ORDER BY 标准化 (expr 包裹)
  │
  ▼
[Stage 3: build]
  canonical JSON → QueryPlan AST
  使用 serde 的 #[serde(tag = "type")] 反序列化
  │
  ▼
[Stage 4: fix]
  自动修复安全默认值:
  - 缺失 alias → None
  - limit=0 → None (PostgreSQL 不支持)
  - select=[] → ["*"] (兜底)
  │
  ▼
[Stage 5: validate (parser_v2)]
  语义校验:
  - SELECT 不能为空
  - JOIN 必须有 ON 条件 (CROSS JOIN 除外)
  - LIMIT 不能为 0
  │
  ▼
[Stage 6: optimize (parser_v2)]
  谓词简化 / 投影裁剪
  │
  ▼
QueryPlan
```

### normalize 子阶段执行顺序

```rust
// crates/vlorql-llm/src/parser_v2/normalize/pipeline.rs
changed |= aliases::normalize_field_names(val);  // 先跑：字段名标准化
changed |= array::normalize(val);                // 再跑：数组结构
changed |= select::normalize(val);                // SELECT 结构
changed |= table::normalize(val);                // FROM 结构
changed |= where_::normalize(val);               // WHERE 结构
changed |= join::normalize(val);                 // JOIN 结构
changed |= query::normalize(val);                // 顶层清理
changed |= operators::normalize(val);            // 操作符
changed |= value::normalize(val);                // 数据类型
changed |= expr::normalize(val);                 // 表达式标签
changed |= order::normalize(val);                // ORDER BY
```

---

## 4. 验证流水线 (vlorql-core)

```text
[ValidationPipeline]
  │
  ├─ Stage 1: Schema Validation
  │    检查表和列是否存在
  │    检查列引用是否正确 (table.column)
  │    检查 JOIN 条件中的列是否合法
  │
  ├─ Stage 2: Policy Validation
  │    检查表是否被策略禁止
  │    检查列是否被策略禁止
  │
  ├─ Stage 3: Operand Type-Check
  │    检查表达式类型兼容性
  │    检查函数参数类型
  │    检查聚合与 GROUP BY 匹配
  │    检查比较操作符两侧类型一致
  │
  └─ Stage 4: Dialect Validation
       检查 CTE 是否被方言支持
       检查 JOIN 类型是否被允许
       检查函数是否在被允许列表中
       检查 DISTINCT 是否被方言支持
```

### ValidatedPlan / OptimizedPlan

```rust
pub struct ValidatedPlan(pub Arc<QueryPlan>);   // 验证通过标记
pub struct OptimizedPlan(ValidatedPlan);        // 验证+优化标记
```

---

## 5. 编译器架构

```text
QueryPlan
  │
  ▼
QueryBuilder::new(plan, dialect, quote_style)
  │  ├ 参数去重缓存 (param_cache)
  │  └ 别名栈 (alias_stack)
  │
  ├─ build_query() → 递归渲染
  │    ├ CTE → WITH/WITH RECURSIVE
  │    ├ SELECT → select 列表
  │    ├ FROM → 表 + 别名
  │    ├ JOIN → JOIN 类型 + ON
  │    ├ WHERE → 谓词
  │    ├ GROUP BY → 表达式列表
  │    ├ HAVING → 谓词
  │    ├ ORDER BY → 表达式 + 方向
  │    ├ LIMIT / OFFSET → 分页
  │    ├ DISTINCT → DISTINCT / DISTINCT ON
  │    └ SET OPERATION → UNION / UNION ALL / INTERSECT / EXCEPT
  │
  └─ Result: (String, Vec<Parameter>)

SqlCompiler trait
  ├ PostgresCompiler  → $1, $2, ...  + " 双引号
  ├ MySqlCompiler     → ?, ?, ...    + ` 反引号
  └ SqliteCompiler    → ?, ?, ...    + " 双引号
```

---

## 6. 缓存架构

```text
Cache trait
  ├ get(key) → Option<V>
  └ insert(key, value)

实现:
  ├ MemoryCache<K, V>     — 进程内 HashMap + TTL
  └ NoopCache<K, V>       — 空实现 (disable)

缓存实例:
  ├ SchemaCache     — SchemaSnapshot 版本化缓存
  ├ PromptCache     — 系统提示词缓存 (schema+方言+策略哈希)
  └ CompileCache    — 编译结果缓存 (plan 哈希 + 方言)
```

### PromptCache Key 组成

```rust
pub struct PromptCacheKey {
    schema_version: String,   // Schema 版本号
    dialect_hash: u64,        // DialectProfile 哈希
    policy_hash: u64,         // PolicyConfig 哈希
    table_hash: u64,          // 相关表集合哈希
}
```

---

## 7. 策略引擎

```text
PolicyConfig
  ├ global_denied_columns: Vec<String>   // 全局禁止列
  └ table_policies: HashMap<String, TablePolicy>

TablePolicy
  ├ allowed: bool                        // 表是否可用
  ├ allowed_columns: Option<Vec<String>> // 可用列白名单
  └ denied_columns: Vec<String>          // 禁止列黑名单

PolicyEngine
  └ validate(plan, schema) → Result<(), Vec<PolicyError>>
```

---

## 8. 错误系统

```text
VlorQLError
  ├ Validation { kind: ValidationErrorKind, details }
  │   ├ InvalidJson
  │   ├ MissingField { field }
  │   ├ InvalidTable { table, available }
  │   ├ InvalidColumn { table, column, available }
  │   ├ InvalidFunction { function, allowed }
  │   ├ TypeMismatch { expected, found, expr }
  │   ├ DialectFeatureDisabled { feature }
  │   ├ TooManyJoins { actual, max }
  │   ├ AggregationMismatch { message }
  │   └ MultipleErrors { count }
  │
  ├ Schema { kind: SchemaErrorKind, details }
  │   ├ ColumnNotFound { table, column, available }
  │   ├ TableNotFound { table, available }
  │   ├ AmbiguousColumn { column, candidates }
  │   └ TableNotInScope { table, in_scope }
  │
  ├ Policy { kind: PolicyErrorKind, details }
  │   ├ TableDenied { table }
  │   └ ColumnDenied { table, column }
  │
  ├ Compilation { kind: CompilationErrorKind, details }
  │   ├ UnsupportedFeature { feature }
  │   └ InvalidLimit { limit }
  │
  ├ Llm { kind: LlmErrorKind, details }
  │   ├ ApiError { status, message }
  │   ├ ParseError { details }
  │   └ EmptyResponse
  │
  └ Config { kind: ConfigErrorKind, details }
```

### 可重试性策略

```rust
// Schema 错误中可重试的:
ColumnNotFound, TableNotInScope, TableNotFound

// Validation 错误中可重试的:
InvalidJson, MissingField, InvalidTable, InvalidColumn,
InvalidFunction, TypeMismatch, AggregationMismatch

// LLM 错误: 全部可重试
// Policy/Compilation/Config 错误: 不可重试
```
