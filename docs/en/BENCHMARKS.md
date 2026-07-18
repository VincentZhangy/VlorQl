# VlorQl Performance Baselines

Baseline numbers captured with `cargo bench -p vlorql-core` on the commit that
introduced the three Criterion benchmarks. Re-run on the same machine before
comparing; numbers below are in nanoseconds (or Kelem/s for the throughput
bench). All measurements pass their respective targets by a wide margin.

Hardware / environment used for the baseline: Linux x86_64, release profile,
gnuplot absent (criterion fell back to the plotters backend — numbers unaffected).

## Targets vs. observed

| Benchmark                                    | Target          | Observed (median) | Headroom |
| -------------------------------------------- | --------------- | ----------------- | -------- |
| `validate/1000_tables_50_cols`               | < 10 ms         | **6.42 µs**       | ~1 550×  |
| `query_build/postgres`                       | < 5 ms          | **21.1 µs**       | ~237×    |
| `query_build/sqlite`                         | < 5 ms          | **20.1 µs**       | ~249×    |
| `concurrent_throughput/postgres`             | ≥ 1 000 QPS     | **325.3 k QPS**   | ~325×    |
| `concurrent_throughput/sqlite`               | ≥ 1 000 QPS     | **296.7 k QPS**   | ~297×    |

## 1. `benches/schema_validation.rs` — `validate/1000_tables_50_cols`

Generates a `SchemaSnapshot` with 1 000 tables × 50 columns each (50 000
columns total) and runs `ValidationPipeline::validate` on a plan that selects
10 columns across 5 tables (one FROM + four INNER JOINs).

```
time:   [6.4219 µs 6.4541 µs 6.4903 µs]
```

Validation is dominated by the indexed `HashMap<String, usize>` lookup in
`SchemaSnapshot::get_table`; the per-column scan in `get_column` runs against
50-entry vectors so it stays in cache.

## 2. `benches/query_compilation.rs` — `query_build/{postgres,sqlite}`

Builds a deliberately heavyweight plan and renders it with `QueryBuilder::build`:

- 3 nested CTEs (the outermost CTE itself contains another CTE),
- 4 joins (INNER / LEFT / RIGHT / FULL),
- WHERE combining `AND`, `OR`, `IN`, `BETWEEN`, `LIKE`, `IS NULL`,
- multi-column `GROUP BY`,
- `HAVING` with `COUNT(...)`,
- `ORDER BY` mixing `SUM(...) DESC` with an ascending column,
- `LIMIT 50 OFFSET 100`.

```
query_build/postgres    time:   [20.924 µs 21.114 µs 21.315 µs]
query_build/sqlite      time:   [20.022 µs 20.138 µs 20.265 µs]
```

Postgres and SQLite are within 1 µs of each other because the only dialect
differences are placeholder style and the `(offset, limit)` vs. `LIMIT/OFFSET`
swap — both cheap relative to the bulk of the rendering work.

## 3. `benches/concurrent.rs` — `concurrent_throughput/{postgres,sqlite}`

LLM calls are mocked out (they would otherwise dominate wall-clock). Each
iteration spawns 100 `tokio::task::spawn` futures on a 4-worker
`tokio::runtime::Builder::new_multi_thread` runtime; each task runs
`ValidationPipeline::validate` followed by `QueryBuilder::build`. `Throughput::Elements(100)`
makes criterion report elements-per-second, which equals QPS here.

```
concurrent_throughput/postgres
    time:   [305.11 µs 307.44 µs 309.94 µs]
    thrpt:  [322.64 Kelem/s 325.27 Kelem/s 327.75 Kelem/s]

concurrent_throughput/sqlite
    time:   [329.23 µs 337.09 µs 346.82 µs]
    thrpt:  [288.33 Kelem/s 296.65 Kelem/s 303.74 Kelem/s]
```

The validator + compiler is essentially CPU-bound on tiny inputs, so the
multi-threaded tokio runtime keeps every core saturated and delivers ~300 k
QPS on a 4-worker setup — three orders of magnitude above the 1 000 QPS
target.

## How to reproduce

```bash
cargo bench --bench schema_validation -p vlorql-core
cargo bench --bench query_compilation  -p vlorql-core
cargo bench --bench concurrent         -p vlorql-core
```

Criterion writes machine-readable JSON estimates to
`target/criterion/<group>/<bench>/new/estimates.json` and an HTML report to
`target/criterion/report/index.html`. To compare against this baseline,
re-run after checking out the baseline commit and then run the suite again on
the new commit — criterion's `compare` subcommand will flag regressions in the
HTML report.

## When to re-baseline

- Any change to `SchemaSnapshot`, `ValidationPipeline`, or `validate_schema`.
- Any change to `QueryBuilder`, `CompiledQuery`, or the per-dialect compilers.
- Any change to the way the runtime shares schema/policy state across tasks
  (e.g. swapping `Arc<SchemaSnapshot>` for a per-task copy).
- New plan-shape optimizations that change allocation behaviour for `String`
  in `build_query`.

## Files touched

- `Cargo.toml` — added `criterion = { version = "0.5", features = ["html_reports"] }`
  to `[workspace.dependencies]`.
- `crates/vlorql-core/Cargo.toml` — added `criterion`, `tokio`, `futures`,
  `serde_json` to `[dev-dependencies]` and three `[[bench]]` entries with
  `harness = false`.
- `crates/vlorql-core/benches/schema_validation.rs` — new.
- `crates/vlorql-core/benches/query_compilation.rs` — new.
- `crates/vlorql-core/benches/concurrent.rs` — new.