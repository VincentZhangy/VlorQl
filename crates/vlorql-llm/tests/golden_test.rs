//! Golden tests: verify the V2 pipeline against realistic LLM outputs.
//!
//! Each test case runs the full pipeline (recover → normalize → build → fix → validate → optimize)
//! and checks the resulting QueryPlan against expected values.
//!
//! These tests serve as regression tests for model-specific output formats.
//! When a new model is added, add a new test case to `tests/golden/mod.rs`.

use vlorql_llm::parser_v2::pipeline::parse_query_plan;

mod golden;

/// Run a single golden test case.
fn run_golden_test(case: &golden::GoldenTestCase) {
    let result = parse_query_plan(case.input);
    let plan = match result {
        Ok(plan) => plan,
        Err(e) => {
            panic!(
                "Golden test '{}' failed to parse:\n  Input: {}\n  Error: {}",
                case.name,
                case.input,
                e,
            );
        }
    };

    // Check SELECT length.
    assert_eq!(
        plan.select.len(),
        case.expected_select_len,
        "{}: SELECT length mismatch",
        case.name,
    );

    // Check FROM table.
    assert_eq!(
        plan.from.table,
        case.expected_from_table,
        "{}: FROM table mismatch",
        case.name,
    );

    // Check WHERE.
    assert_eq!(
        plan.r#where.is_some(),
        case.expected_has_where,
        "{}: WHERE presence mismatch (expected_has_where={})",
        case.name,
        case.expected_has_where,
    );

    // Check ORDER BY.
    assert_eq!(
        plan.order_by.is_some(),
        case.expected_has_order_by,
        "{}: ORDER BY presence mismatch",
        case.name,
    );

    // Check JOINs.
    assert_eq!(
        plan.joins.is_some(),
        case.expected_has_joins,
        "{}: JOINs presence mismatch",
        case.name,
    );

    // Check CTEs.
    assert_eq!(
        plan.ctes.is_some(),
        case.expected_has_ctes,
        "{}: CTEs presence mismatch",
        case.name,
    );

    // Check LIMIT.
    assert_eq!(
        plan.limit,
        case.expected_limit,
        "{}: LIMIT mismatch",
        case.name,
    );

    // Check FROM alias.
    assert_eq!(
        plan.from.alias.as_deref(),
        case.expected_alias,
        "{}: FROM alias mismatch",
        case.name,
    );
}

// ── Individual test cases ──────────────────────────────────────────
// Each test case is a separate function so failures are reported individually.

#[test]
fn golden_openai_simple_star() {
    run_golden_test(&golden::ALL_CASES[0]);
}

#[test]
fn golden_openai_with_where() {
    run_golden_test(&golden::ALL_CASES[1]);
}

#[test]
fn golden_openai_with_join() {
    run_golden_test(&golden::ALL_CASES[2]);
}

#[test]
fn golden_deepseek_filter_alias() {
    run_golden_test(&golden::ALL_CASES[3]);
}

#[test]
fn golden_deepseek_with_operator_aliases() {
    run_golden_test(&golden::ALL_CASES[4]);
}

#[test]
fn golden_qwen_string_projections() {
    run_golden_test(&golden::ALL_CASES[5]);
}

#[test]
fn golden_qwen_array_wrapped() {
    run_golden_test(&golden::ALL_CASES[6]);
}

#[test]
fn golden_claude_standard() {
    run_golden_test(&golden::ALL_CASES[7]);
}

#[test]
fn golden_claude_with_limit_and_offset() {
    run_golden_test(&golden::ALL_CASES[8]);
}

#[test]
fn golden_glm_conditions_and_fields() {
    run_golden_test(&golden::ALL_CASES[9]);
}

#[test]
fn golden_glm_with_group_by() {
    run_golden_test(&golden::ALL_CASES[10]);
}

#[test]
fn golden_llama_markdown_fence() {
    run_golden_test(&golden::ALL_CASES[11]);
}

#[test]
fn golden_llama_where_array_with_garbage() {
    run_golden_test(&golden::ALL_CASES[12]);
}

#[test]
fn golden_mistral_standard() {
    run_golden_test(&golden::ALL_CASES[13]);
}

#[test]
fn golden_mistral_with_join_and_cte() {
    run_golden_test(&golden::ALL_CASES[14]);
}

#[test]
fn golden_edge_empty_group_by() {
    run_golden_test(&golden::ALL_CASES[15]);
}

#[test]
fn golden_edge_missing_data_type() {
    run_golden_test(&golden::ALL_CASES[16]);
}

#[test]
fn golden_edge_limit_zero_fixed() {
    run_golden_test(&golden::ALL_CASES[17]);
}

// ── Iterate all test cases at once (convenience for bulk checks) ───

#[test]
fn golden_all_cases() {
    let mut errors = Vec::new();
    for (i, case) in golden::ALL_CASES.iter().enumerate() {
        let result = std::panic::catch_unwind(|| {
            run_golden_test(case);
        });
        if let Err(e) = result {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                format!("{:?}", e)
            };
            errors.push(format!("  [{}/{}] {}: {}", i, golden::ALL_CASES.len(), case.name, msg));
        }
    }
    if !errors.is_empty() {
        panic!(
            "Golden test failures:\n{}\n",
            errors.join("\n"),
        );
    }
}