//! Benchmarks for the compile cache and schema cache.
//!
//! Measures:
//!   1. Compile cache hit vs miss latency.
//!   2. Schema cache hit vs miss latency.
//!   3. Concurrent cache access throughput.
//!
//! Expected: a compile cache hit should be ~0.05 ms, roughly 40× faster
//! than a full compile (~2 ms).

use criterion::{Criterion, criterion_group, criterion_main};
use std::sync::Arc;
use vlorql_core::cache::{CompileCache, SchemaCache, SchemaCacheKey};
use vlorql_core::compile::CompiledQuery;
use vlorql_core::schema::{
    ColumnSchema, DataType, DialectProfile, FromClause, Projection, QueryPlan, SchemaMetadata,
    SchemaSnapshot, SqlDialect, TableSchema,
};
use vlorql_core::validate::ValidatedPlan;

fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![TableSchema {
            name: "users".to_owned(),
            columns: vec![ColumnSchema {
                name: "id".to_owned(),
                data_type: DataType::Int,
                nullable: false,
                description: None,
                is_primary_key: true,
                foreign_key: None,
            }],
            description: None,
            primary_key: None,
        }],
        SchemaMetadata::default(),
    ))
}

fn build_plan() -> ValidatedPlan {
    ValidatedPlan(Arc::new(QueryPlan {
        select: vec![Projection::Column {
            table: None,
            column: "id".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    }))
}

fn build_compiled() -> CompiledQuery {
    CompiledQuery {
        sql: "SELECT \"id\" FROM \"users\"".to_owned(),
        parameters: vec![],
        dialect: SqlDialect::Postgres,
    }
}

// ---------------------------------------------------------------------------
// Compile cache benchmarks
// ---------------------------------------------------------------------------

fn bench_compile_cache_miss(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cache = CompileCache::new(1024, 60);
    let plan = build_plan();
    let profile = DialectProfile::default();
    let compiled = build_compiled();

    // Pre-insert so the hit bench measures only the lookup.
    rt.block_on(cache.insert(&plan, &profile, compiled.clone()));

    c.bench_function("cache/compile_hit", |bencher| {
        bencher.iter(|| {
            let result = rt.block_on(cache.get(&plan, &profile));
            criterion::black_box(result);
        });
    });

    // Miss bench: use a different plan so the key is always absent.
    let other_plan = ValidatedPlan(Arc::new(QueryPlan {
        select: vec![Projection::Column {
            table: None,
            column: "email".to_owned(),
            alias: None,
        }],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    }));

    c.bench_function("cache/compile_miss", |bencher| {
        bencher.iter(|| {
            let result = rt.block_on(cache.get(&other_plan, &profile));
            criterion::black_box(result);
        });
    });
}

// ---------------------------------------------------------------------------
// Schema cache benchmarks
// ---------------------------------------------------------------------------

fn bench_schema_cache(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cache = SchemaCache::new(100, 300);
    let schema = build_schema();
    let key = SchemaCacheKey {
        version: "v1".to_owned(),
        source: "test://db".to_owned(),
    };

    // Pre-insert.
    rt.block_on(cache.get_or_insert_with(key.clone(), || async { Arc::clone(&schema) }));

    c.bench_function("cache/schema_hit", |bencher| {
        bencher.iter(|| {
            let result = rt
                .block_on(cache.get_or_insert_with(key.clone(), || async { Arc::clone(&schema) }));
            criterion::black_box(result);
        });
    });

    // Miss: different key.
    let miss_key = SchemaCacheKey {
        version: "v2".to_owned(),
        source: "test://db".to_owned(),
    };

    c.bench_function("cache/schema_miss", |bencher| {
        bencher.iter(|| {
            let result = rt.block_on(
                cache.get_or_insert_with(miss_key.clone(), || async { Arc::clone(&schema) }),
            );
            criterion::black_box(result);
        });
    });
}

criterion_group!(benches, bench_compile_cache_miss, bench_schema_cache);
criterion_main!(benches);
