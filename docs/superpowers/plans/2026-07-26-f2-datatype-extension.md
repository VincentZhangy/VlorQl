# F2: DataType 扩展（Decimal / Array / Jsonb / Blob）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development / superpowers:executing-plans。串行执行 F2-1 → F2-2。

**Goal:** 给 `DataType` 增加 Decimal / Array / Jsonb / Blob 四个类型，接入校验/类型系统/编译器/提示词/解析各环节，并修复 `blob`/`date` 预存不一致。

**Architecture:** 因 `DataType` derive `Copy`，四个新变体必须是**无字段单元变体**（Decimal 无 precision/scale，Array 无元素类型）。**不加 `serde(other)` 兜底**——未知类型字符串仍严格报错（保持 security/malformed_json 的拒绝保证）。F2-1 原子完成枚举 + 全部穷尽 match + 类型语义（必须一起编译通过）；F2-2 处理 normalize 别名层。

**Tech Stack:** Rust (edition 2024)。分支 `feat/0.4.0`。

## Global Constraints

- CI 全绿：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo check -p vlorql --examples`、docs。
- `RUSTFLAGS: -D warnings`；`#![deny(missing_docs)]`（新枚举变体必须有 doc 注释）。
- **不加 `#[serde(other)]`**（保持严格反序列化）。四个新变体为无字段单元变体，保持 `DataType: Copy`。
- 三方言语法正确（Postgres/MySQL/SQLite）。
- TDD：先失败测试 → 确认失败 → 实现 → 确认通过 → 提交。

---

## File Structure

| 文件 | 责任 | 任务 |
|------|------|------|
| `crates/vlorql-core/src/schema/types.rs`（枚举 :24-43） | 加 4 个单元变体 | F2-1 |
| `crates/vlorql-core/src/validate/operand.rs`（:618/643/657/673） | literal_matches_type、data_type_name、is_numeric、numeric_result_type | F2-1 |
| `crates/vlorql-core/src/prompt/builder.rs`（:763） | data_type_name（重复实现，同步） | F2-1 |
| `crates/vlorql-core/src/compile/builder.rs`（:120-127） | CTE 给 Decimal 加 CAST AS NUMERIC | F2-1 |
| `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs`（parse_data_type :172） | 加 decimal/array/jsonb/blob/date 字符串分支 | F2-1 |
| `crates/vlorql-llm/src/parser_v2/normalize/value.rs`（别名 :9、is_canonical :123） | 别名 + is_canonical + 修 blob/date 不一致 | F2-2 |

---

## Task F2-1: 枚举 + 全部穷尽 match + 类型语义（原子可编译）

**必须一次改完并编译通过**（三处穷尽 match 不加分支则 `cargo build` 失败）。

**Interfaces:** `DataType` 新增单元变体 `Decimal`、`Array`、`Jsonb`、`Blob`（serde snake_case → "decimal"/"array"/"jsonb"/"blob"）。

- [ ] **Step 1: 写失败测试**

在 `crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs` 的测试模块（或新建）新增，验证解析：
```rust
#[test]
fn parse_new_data_types() {
    assert_eq!(parse_data_type("decimal").unwrap(), vlorql_core::schema::DataType::Decimal);
    assert_eq!(parse_data_type("array").unwrap(), vlorql_core::schema::DataType::Array);
    assert_eq!(parse_data_type("jsonb").unwrap(), vlorql_core::schema::DataType::Jsonb);
    assert_eq!(parse_data_type("blob").unwrap(), vlorql_core::schema::DataType::Blob);
    assert_eq!(parse_data_type("date").unwrap(), vlorql_core::schema::DataType::Date);
}
```
并在 `crates/vlorql-core/src/validate/operand.rs` 测试模块新增类型系统断言（`is_numeric`/`numeric_result_type`/`literal_matches_type` 是模块私有，`use super::*;`）：
```rust
#[test]
fn decimal_is_numeric_and_promotes() {
    assert!(is_numeric(DataType::Decimal));
    // Float > Decimal > Int
    assert_eq!(numeric_result_type(DataType::Int, DataType::Decimal), DataType::Decimal);
    assert_eq!(numeric_result_type(DataType::Decimal, DataType::Float), DataType::Float);
}
#[test]
fn container_literal_matching() {
    assert!(literal_matches_type(&serde_json::json!([1,2]), DataType::Array));
    assert!(literal_matches_type(&serde_json::json!({"a":1}), DataType::Jsonb));
    assert!(literal_matches_type(&serde_json::json!("YWJj"), DataType::Blob));
    assert!(literal_matches_type(&serde_json::json!(3.14), DataType::Decimal));
    assert!(!literal_matches_type(&serde_json::json!(3), DataType::Array));
}
```

- [ ] **Step 2: 运行确认失败**

`cargo test -p vlorql-llm --lib parse_new_data_types` 与 `cargo build -p vlorql-core` —— 预期编译失败（DataType 无这些变体 / 穷尽 match non-exhaustive）。

- [ ] **Step 3: 加枚举变体（types.rs，Uuid 之后）**
```rust
    /// Fixed-point decimal number (arbitrary precision). Distinct from
    /// [`DataType::Float`] so decimal semantics are preserved.
    Decimal,
    /// Ordered collection of values (SQL array). Element type is not tracked.
    Array,
    /// Binary JSON (PostgreSQL `JSONB`).
    Jsonb,
    /// Arbitrary binary data (`BLOB` / `BYTEA`).
    Blob,
```

- [ ] **Step 4: 三处穷尽 match + 语义函数（operand.rs）**

`literal_matches_type`（:618-628）加分支：
```rust
        DataType::Decimal => value.is_number() || value.is_string(),
        DataType::Array => value.is_array(),
        DataType::Jsonb => true,
        DataType::Blob => value.is_string(),
```
`data_type_name`（:643-654）加：`DataType::Decimal => "decimal", DataType::Array => "array", DataType::Jsonb => "jsonb", DataType::Blob => "blob",`
`is_numeric`（:657）：`matches!(data_type, DataType::Int | DataType::Float | DataType::Decimal)`
`numeric_result_type`（:673-680）改为含 Decimal 提升（Float > Decimal > Int）：
```rust
fn numeric_result_type(left: DataType, right: DataType) -> DataType {
    if left == DataType::Float || right == DataType::Float {
        DataType::Float
    } else if left == DataType::Decimal || right == DataType::Decimal {
        DataType::Decimal
    } else if left == DataType::Int || right == DataType::Int {
        DataType::Int
    } else {
        DataType::Null
    }
}
```
> `is_string_compatible`、`types_compatible` 无需改：Array/Jsonb/Blob 默认不匹配 string（正确），同类型比较经 `left==right` 已放行，Decimal 经更新后的 `is_numeric` 自动纳入数值比较。

- [ ] **Step 5: prompt/builder.rs data_type_name（:763-774）同步加同样 4 个分支**（"decimal"/"array"/"jsonb"/"blob"）。

- [ ] **Step 6: 编译器 CTE CAST（compile/builder.rs:121-126）**

在 match 里 `DataType::Boolean` 分支后加：
```rust
                        DataType::Decimal => write!(buf, "CAST({placeholder} AS NUMERIC)"),
```
（Array/Jsonb/Blob 保持走 `_ =>` 裸占位符，交由驱动/DB 处理。）

- [ ] **Step 7: parse_data_type（expr_builder.rs:174-182）加分支**
```rust
        "decimal" => Ok(Decimal),
        "array" => Ok(Array),
        "jsonb" => Ok(Jsonb),
        "blob" => Ok(Blob),
        "date" => Ok(Date),
```

- [ ] **Step 8: 运行确认通过 + 全量校验 + 提交**
```
cargo test -p vlorql-core -p vlorql-llm
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/vlorql-core/src/schema/types.rs crates/vlorql-core/src/validate/operand.rs \
        crates/vlorql-core/src/prompt/builder.rs crates/vlorql-core/src/compile/builder.rs \
        crates/vlorql-llm/src/parser_v2/builder/expr_builder.rs
git commit -m "feat(schema): DataType 新增 Decimal/Array/Jsonb/Blob + 类型系统接入 (F2-1)"
```

---

## Task F2-2: normalize 别名 + is_canonical（修 blob/date 不一致）

**Files:** `crates/vlorql-llm/src/parser_v2/normalize/value.rs`（`DATA_TYPE_ALIASES` :9、`is_canonical` :123-128）

**问题：** 别名表把 `decimal/numeric → float`（引入 Decimal 后应改映射到 decimal，否则 Decimal 永远收不到输入）；`is_canonical` 列了 `blob` 但此前枚举没有（现已加，合法了），且缺 decimal/array/jsonb。

- [ ] **Step 1: 写失败测试（value.rs mod tests）**
```rust
#[test]
fn decimal_alias_maps_to_decimal_not_float() {
    assert_eq!(resolve_data_type("decimal"), Some("decimal"));
    assert_eq!(resolve_data_type("numeric"), Some("decimal"));
    // 真正的浮点别名仍映射到 float
    assert_eq!(resolve_data_type("double precision"), Some("float"));
}
#[test]
fn new_types_are_canonical() {
    assert!(is_canonical("decimal"));
    assert!(is_canonical("array"));
    assert!(is_canonical("jsonb"));
    assert!(is_canonical("blob"));
}
```

- [ ] **Step 2: 确认失败**

`cargo test -p vlorql-llm --lib normalize::value` → FAIL（decimal→float、is_canonical 不认新类型）。

- [ ] **Step 3: 改别名表（value.rs:24-25）**

把 `("decimal", "float"), ("numeric", "float"),` 改为 `("decimal", "decimal"), ("numeric", "decimal"),`（`real/double/double precision → float` 保持不变）。并在合适分组加 `("bytea", "blob"),`。

- [ ] **Step 4: 改 is_canonical（:123-128）**

在集合里加 `decimal | array | jsonb`（`blob` 已在，现已合法，保留）：
```rust
    matches!(
        dt,
        "int" | "string" | "float" | "boolean" | "timestamp" | "null" | "json" | "uuid"
            | "blob" | "decimal" | "array" | "jsonb"
    )
```
> `date` 仍经 `date → timestamp` 别名折叠（保持既有行为，不在本任务改），故不加入 is_canonical——与折叠行为一致。

- [ ] **Step 5: 确认通过 + 校验 + 提交**
```
cargo test -p vlorql-llm
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/vlorql-llm/src/parser_v2/normalize/value.rs
git commit -m "feat(normalize): decimal/numeric→decimal 别名 + is_canonical 纳入新类型 (F2-2)"
```

---

## Self-Review

- **Spec coverage**：4 个新变体（Decimal/Array/Jsonb/Blob）在枚举、3 处穷尽 match、类型语义（is_numeric/numeric_result_type/literal_matches_type）、编译器 CTE、parse_data_type、normalize 别名、is_canonical 全部接入。不加 Unknown。
- **预存不一致修复**：blob（加入枚举 + parse + 保留 is_canonical）、parse_data_type 补 date。
- **Copy 保持**：全部无字段单元变体。
- **依赖**：F2-1 必须原子编译通过（穷尽 match）；F2-2 依赖 F2-1（用到新变体的字符串形式）。串行 F2-1→F2-2。
- **未覆盖（有意）**：Array 的有序比较（< >）不特殊处理（同类型经 types_compatible 放行）；MySQL/SQLite 的 Array/Jsonb/Blob 专门渲染（保持裸占位符 fallback）；date→timestamp 折叠行为不变。这些留待后续按需。
