# VlorQl 性能基准

基准数据通过 `cargo bench -p vlorql-core` 在引入三个 Criterion 基准测试的提交上捕获。在比较之前请在相同机器上重新运行；以下数据以纳秒为单位（吞吐量基准为 Kelem/s）。所有测量值均以较大余量通过各自的目标。

用于基准测试的硬件/环境：Linux x86_64，release profile，gnuplot 缺失（criterion 回退到 plotters 后端——数据不受影响）。

## 目标 vs. 实际观测

| 基准测试 | 目标 | 实际观测（中位数） | 余量 |
|----------|------|-------------------|------|
| `validate/1000_tables_50_cols` | < 10 ms | **6.42 µs** | ~1 550× |
| `query_build/postgres` | < 5 ms | **21.1 µs** | ~237× |
| `query_build/sqlite` | < 5 ms | **20.1 µs** | ~249× |
| `concurrent_throughput/postgres` | ≥ 1 000 QPS | **325.3 k QPS** | ~325× |
| `concurrent_throughput/sqlite` | ≥ 1 000 QPS | **296.7 k QPS** | ~297× |

## 1. `benches/schema_validation.rs` — `validate/1000_tables_50_cols`

生成一个包含 1 000 个表 × 50 列（共 50 000 列）的 `SchemaSnapshot`，并在一个选择 10 列跨 5 个表（一个 FROM + 四个 INNER JOIN）的计划上运行 `ValidationPipeline::validate`。

```
time:   [6.4219 µs 6.4541 µs 6.4903 µs]
```

验证过程主要由 `SchemaSnapshot::get_table` 中的索引化 `HashMap<String, usize>` 查询主导；`get_column` 中的逐列扫描针对 50 条记录的向量，因此保持在缓存中。

## 2. `benches/query_compilation.rs` — `query_build/{postgres,sqlite}`

构建一个故意复杂的计划并使用 `QueryBuilder::build` 渲染：

- 3 层嵌套 CTE（最外层的 CTE 本身包含另一个 CTE），
- 4 个 JOIN（INNER / LEFT / RIGHT / FULL），
- WHERE 组合了 `AND`、`OR`、`IN`、`BETWEEN`、`LIKE`、`IS NULL`，
- 多列 `GROUP BY`，
- 带 `COUNT(...)` 的 `HAVING`，
- `ORDER BY` 混合了 `SUM(...) DESC` 和一个升序列，
- `LIMIT 50 OFFSET 100`。

```
query_build/postgres    time:   [20.924 µs 21.114 µs 21.315 µs]
query_build/sqlite      time:   [20.022 µs 20.138 µs 20.265 µs]
```

Postgres 和 SQLite 的差异在 1 µs 以内，因为唯一的方言差异是占位符风格和 `(offset, limit)` 与 `LIMIT/OFFSET` 的互换——两者相对于渲染工作的主体来说都很廉价。

## 3. `benches/concurrent.rs` — `concurrent_throughput/{postgres,sqlite}`

LLM 调用被模拟掉（否则它们会主导挂钟时间）。每次迭代在 4 工作线程的 `tokio::runtime::Builder::new_multi_thread` 运行时上生成 100 个 `tokio::task::spawn` 未来；每个任务运行 `ValidationPipeline::validate` 后跟 `QueryBuilder::build`。`Throughput::Elements(100)` 使 criterion 报告每秒元素数，在此处等于 QPS。

```
concurrent_throughput/postgres
    time:   [305.11 µs 307.44 µs 309.94 µs]
    thrpt:  [322.64 Kelem/s 325.27 Kelem/s 327.75 Kelem/s]

concurrent_throughput/sqlite
    time:   [329.23 µs 337.09 µs 346.82 µs]
    thrpt:  [288.33 Kelem/s 296.65 Kelem/s 303.74 Kelem/s]
```

验证器 + 编译器在小型输入上基本上是 CPU 密集型的，因此多线程 tokio 运行时保持每个核心饱和，在 4 工作线程设置上提供约 300 k QPS——比 1 000 QPS 的目标高出三个数量级。

## 如何复现

```bash
cargo bench --bench schema_validation -p vlorql-core
cargo bench --bench query_compilation  -p vlorql-core
cargo bench --bench concurrent         -p vlorql-core
```

Criterion 将机器可读的 JSON 估计值写入 `target/criterion/<group>/<bench>/new/estimates.json`，将 HTML 报告写入 `target/criterion/report/index.html`。要与此基准进行比较，请先检出基准提交后重新运行，然后在新的提交上再次运行套件——criterion 的 `compare` 子命令会在 HTML 报告中标记回归。

## 何时需要重新基准

- 任何对 `SchemaSnapshot`、`ValidationPipeline` 或 `validate_schema` 的更改。
- 任何对 `QueryBuilder`、`CompiledQuery` 或方言特定编译器的更改。
- 任何对运行时跨任务共享 schema/策略状态的方式的更改（例如将 `Arc<SchemaSnapshot>` 替换为每个任务副本）。
- 改变 `build_query` 中 `String` 分配行为的新计划形状优化。

## 涉及的文件

- `Cargo.toml` — 在 `[workspace.dependencies]` 中添加了 `criterion = { version = "0.5", features = ["html_reports"] }`。
- `crates/vlorql-core/Cargo.toml` — 在 `[dev-dependencies]` 中添加了 `criterion`、`tokio`、`futures`、`serde_json`，以及三个带 `harness = false` 的 `[[bench]]` 条目。
- `crates/vlorql-core/benches/schema_validation.rs` — 新增。
- `crates/vlorql-core/benches/query_compilation.rs` — 新增。
- `crates/vlorql-core/benches/concurrent.rs` — 新增。