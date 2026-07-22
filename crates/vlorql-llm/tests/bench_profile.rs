//! Performance profiling for the V2 pipeline stages.
//!
//! Measures the time spent in each stage of the pipeline:
//! recover → normalize → build → fix → validate → optimize
//!
//! Run with: `cargo test -p vlorql-llm --test bench_profile -- --nocapture`

use std::time::Instant;

use vlorql_llm::parser_v2::builder::query_builder;
use vlorql_llm::parser_v2::fix::fixer;
use vlorql_llm::parser_v2::normalize::pipeline as normalize_pipeline;
use vlorql_llm::parser_v2::optimize::optimize as optimize_plan;
use vlorql_llm::parser_v2::recover::extract_json_content;
use vlorql_llm::parser_v2::validate::validator;

/// Number of iterations for each benchmark.
const ITERATIONS: usize = 1000;

/// A realistic complex LLM output with multiple features.
const COMPLEX_INPUT: &str = r#"{
    "projection": ["id", "name", "email", {"column": "created_at"}],
    "source": "users",
    "filter": [
        {
            "type": "and",
            "left": {"type": "comparison", "left": {"column": "age"}, "operator": ">", "right": {"value": 18, "data_type": "integer"}},
            "right": {
                "type": "or",
                "left": {"type": "comparison", "left": {"column": "status"}, "op": "=", "right": {"value": "active"}},
                "right": {"type": "comparison", "left": {"column": "status"}, "op": "=", "right": {"value": "pending"}}
            }
        }
    ],
    "sort": [{"expr": {"column": "name"}, "descending": true}],
    "limit": 10,
    "right": {"column": "id"},
    "expr": {"column": "name"},
    "descending": true
}"#;

/// A simple minimal input for baseline measurement.
const SIMPLE_INPUT: &str = r#"{"select":[{"type":"star"}],"from":{"table":"users"}}"#;

/// A realistic messy input with markdown fence and garbage.
const MESSY_INPUT: &str = "Here is the plan:\n```json\n{\"projection\": [{\"column\": \"name\"}, {\"column\": \"email\"}], \"source\": \"employees\", \"filter\": [{\"type\": \"comparison\", \"left\": {\"column\": \"salary\"}, \"operator\": \">\", \"right\": {\"value\": 50000, \"data_type\": \"integer\"}}], \"sort\": [{\"expr\": {\"column\": \"name\"}, \"descending\": true}]}\n```";

struct StageTiming {
    name: &'static str,
    total_ns: u128,
    min_ns: u128,
    max_ns: u128,
    count: usize,
}

impl StageTiming {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            total_ns: 0,
            min_ns: u128::MAX,
            max_ns: 0,
            count: 0,
        }
    }

    fn record(&mut self, ns: u128) {
        self.total_ns += ns;
        self.min_ns = self.min_ns.min(ns);
        self.max_ns = self.max_ns.max(ns);
        self.count += 1;
    }

    fn avg_ns(&self) -> u128 {
        if self.count == 0 {
            0
        } else {
            self.total_ns / self.count as u128
        }
    }

    fn report(&self) {
        println!(
            "  {:12} | avg {:>8} ns | min {:>8} ns | max {:>8} ns | total {:>12} ns",
            self.name,
            self.avg_ns(),
            self.min_ns,
            self.max_ns,
            self.total_ns,
        );
    }
}

fn profile_pipeline(label: &str, input: &str, iterations: usize) {
    // Pre-canonicalize the JSON (shared across iterations).
    let json_str = extract_json_content(input).to_owned();
    let initial_value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let mut stages = vec![
        StageTiming::new("recover"),
        StageTiming::new("normalize"),
        StageTiming::new("build"),
        StageTiming::new("fix"),
        StageTiming::new("validate"),
        StageTiming::new("optimize"),
    ];

    // Warmup: 10 iterations.
    for _ in 0..10 {
        let mut val = initial_value.clone();
        normalize_pipeline::normalize(&mut val);
        let plan = query_builder::build_plan(&val).unwrap();
        let mut plan2 = plan.clone();
        fixer::fix_plan(&mut plan2);
        let _ = validator::validate_plan(&plan2);
        let mut plan3 = plan2.clone();
        optimize_plan(&mut plan3);
    }

    // Benchmark.
    for _ in 0..iterations {
        // Stage 1: Recover
        let start = Instant::now();
        let _json = extract_json_content(input);
        stages[0].record(start.elapsed().as_nanos());

        // Stage 2: Normalize
        let start = Instant::now();
        let mut val = initial_value.clone();
        normalize_pipeline::normalize(&mut val);
        stages[1].record(start.elapsed().as_nanos());

        // Stage 3: Build
        let start = Instant::now();
        let plan = query_builder::build_plan(&val).unwrap();
        stages[2].record(start.elapsed().as_nanos());

        // Stage 4: Fix
        let start = Instant::now();
        let mut plan2 = plan.clone();
        fixer::fix_plan(&mut plan2);
        stages[3].record(start.elapsed().as_nanos());

        // Stage 5: Validate
        let start = Instant::now();
        let _ = validator::validate_plan(&plan2);
        stages[4].record(start.elapsed().as_nanos());

        // Stage 6: Optimize
        let start = Instant::now();
        let mut plan3 = plan2.clone();
        optimize_plan(&mut plan3);
        stages[5].record(start.elapsed().as_nanos());
    }

    // Report.
    println!("\n─── {} ─── ({} iterations)", label, iterations);
    println!(
        "  {:<12} | {:>10} | {:>10} | {:>10} | {:>14}",
        "Stage", "Avg (ns)", "Min (ns)", "Max (ns)", "Total (ns)"
    );
    println!("  {}", "-".repeat(75));
    let total_avg: u128 = stages.iter().map(|s| s.avg_ns()).sum();
    for stage in &stages {
        stage.report();
    }
    println!("  {}", "-".repeat(75));
    println!(
        "  {:12}   avg {:>8} ns (total pipeline)",
        "TOTAL", total_avg
    );
    println!();
}

#[test]
fn bench_profile_complex() {
    profile_pipeline("COMPLEX INPUT", COMPLEX_INPUT, ITERATIONS);
}

#[test]
fn bench_profile_simple() {
    profile_pipeline("SIMPLE INPUT", SIMPLE_INPUT, ITERATIONS);
}

#[test]
fn bench_profile_messy() {
    profile_pipeline("MESSY INPUT", MESSY_INPUT, ITERATIONS);
}

#[test]
fn bench_profile_all() {
    profile_pipeline("COMPLEX INPUT", COMPLEX_INPUT, ITERATIONS / 2);
    profile_pipeline("SIMPLE INPUT", SIMPLE_INPUT, ITERATIONS / 2);
    profile_pipeline("MESSY INPUT", MESSY_INPUT, ITERATIONS / 2);
}
