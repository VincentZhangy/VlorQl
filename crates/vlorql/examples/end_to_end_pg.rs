//! VlorQl 端到端示例：从自然语言 → PostgreSQL 执行
//!
//! # 关键设计
//!
//! 本示例演示 VlorQl 的真实使用方式 —— **您只需要输入自然语言问题**，
//! QueryPlan 的生成、验证、编译全部由框架和 LLM 自动完成，对您完全透明。
//! 您**不需要**手动构建任何 QueryPlan。
//!
//! # 运行方式
//!
//! ## 方式一：真实 LLM（推荐）
//! 设置任意 OpenAI 兼容的 API Key，框架会自动调用 LLM 生成 QueryPlan：
//!
//! ```bash
//! OPENAI_API_KEY="sk-..." \
//!   cargo run --example end_to_end_pg --quiet
//! ```
//!
//! 也支持 DeepSeek / Zhipu / vLLM / Ollama，通过 LLM_PROVIDER 指定：
//! ```bash
//! LLM_PROVIDER=deepseek DEEPSEEK_API_KEY="sk-..." \
//!   cargo run --example end_to_end_pg --quiet
//! ```
//!
//! ## 方式二：离线演示模式（无需 API Key）
//! 不设置任何 API Key，使用内置的 Mock LLM 演示完整流程：
//!
//! ```bash
//! cargo run --example end_to_end_pg --quiet
//! ```
//!
//! ## 可选：连接 PostgreSQL 执行 SQL
//! 在上述命令基础上追加 DATABASE_URL 即可在 PG 上真实执行：
//!
//! ```bash
//! DATABASE_URL="host=localhost user=postgres dbname=test_db" \
//!   OPENAI_API_KEY="sk-..." \
//!   cargo run --example end_to_end_pg --quiet
//! ```
//!
//! # 前置条件（连接 PG 时需要）
//! 启动 PostgreSQL 并创建测试数据库：
//! ```bash
//! docker run -d --name pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
//! createdb test_db  # 或使用已有的数据库
//! ```

use std::error::Error;
use std::sync::Arc;

use bytes::BufMut;
use vlorql::{SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::prompt::PromptBuilder;
use vlorql_core::schema::{
    BinaryOperator, ColumnSchema, CommonTableExpression, ComparisonOperator, DataType, Expression,
    ForeignKey, FromClause, InTarget, JoinClause, JoinType, OrderByTerm, Predicate, Projection,
    QueryPlan, SchemaMetadata, SetOperation, SetOperationClause, TableSchema, WhenThen, WindowSpec,
};
use vlorql_core::validate::ValidatedPlan;
use vlorql_llm::{LlmClient, LlmConfig, LlmProvider, create_llm_client};

use rustls::SignatureScheme;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

/// 跳过所有证书验证（仅用于开发测试，等同于 sslmode=require）
#[derive(Debug)]
struct SkipVerify;

impl ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

// ============================================================================
// 1. 定义数据库 Schema
// ============================================================================

/// 辅助函数：创建一个普通列。
fn col(name: &str, data_type: DataType, description: &str) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type,
        nullable: false,
        description: Some(description.to_owned()),
        is_primary_key: false,
        foreign_key: None,
    }
}

/// 辅助函数：创建一个主键列（`INT` 类型）。
fn pk(name: &str, description: &str) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type: DataType::Int,
        nullable: false,
        description: Some(description.to_owned()),
        is_primary_key: true,
        foreign_key: None,
    }
}

/// 辅助函数：创建一个外键列（`INT` 类型）。
fn fk(name: &str, foreign_table: &str, foreign_column: &str, description: &str) -> ColumnSchema {
    ColumnSchema {
        name: name.to_owned(),
        data_type: DataType::Int,
        nullable: false,
        description: Some(description.to_owned()),
        is_primary_key: false,
        foreign_key: Some(ForeignKey {
            foreign_table: foreign_table.to_owned(),
            foreign_column: foreign_column.to_owned(),
        }),
    }
}

/// 辅助函数：创建一个表。
fn table(name: &str, description: &str, columns: Vec<ColumnSchema>) -> TableSchema {
    TableSchema {
        name: name.to_owned(),
        columns,
        description: Some(description.to_owned()),
        primary_key: Some(vec!["id".to_owned()]),
    }
}

/// 电商数据库 Schema：users、orders、products、order_items、employees 五张表。
fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![
            table(
                "users",
                "应用注册用户",
                vec![
                    pk("id", "用户唯一标识符"),
                    col("name", DataType::String, "用户显示名称"),
                    col("email", DataType::String, "用户邮箱地址"),
                    col("created_at", DataType::String, "用户注册时间 (ISO-8601)"),
                ],
            ),
            table(
                "orders",
                "客户订单记录",
                vec![
                    pk("id", "订单唯一标识符"),
                    fk("user_id", "users", "id", "关联到 users.id"),
                    col("total", DataType::Float, "订单总金额"),
                    col(
                        "status",
                        DataType::String,
                        "订单状态: pending/shipped/completed/cancelled",
                    ),
                    col("created_at", DataType::String, "下单时间 (ISO-8601)"),
                ],
            ),
            table(
                "products",
                "产品目录",
                vec![
                    pk("id", "产品唯一标识符"),
                    col("name", DataType::String, "产品名称"),
                    col("price", DataType::Float, "产品单价"),
                ],
            ),
            table(
                "order_items",
                "订单中的商品明细",
                vec![
                    pk("id", "明细唯一标识符"),
                    fk("order_id", "orders", "id", "关联到 orders.id"),
                    fk("product_id", "products", "id", "关联到 products.id"),
                    col("quantity", DataType::Int, "购买数量"),
                    col("unit_price", DataType::Float, "购买时单价"),
                ],
            ),
            table(
                "employees",
                "公司员工信息（含上下级关系，用于自连接演示）",
                vec![
                    pk("id", "员工唯一标识符"),
                    col("name", DataType::String, "员工姓名"),
                    ColumnSchema {
                        name: "manager_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: true,
                        description: Some("上级员工ID，指向 employees.id，NULL 表示最高级".to_owned()),
                        is_primary_key: false,
                        foreign_key: Some(ForeignKey {
                            foreign_table: "employees".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    col("department", DataType::String, "所属部门"),
                    col("salary", DataType::Float, "月薪"),
                ],
            ),
        ],
        SchemaMetadata {
            version: Some("1.0".to_owned()),
            source: Some("example_ecommerce".to_owned()),
            generated_at: None,
        },
    ))
}

// ============================================================================
// 2. 选择 LLM 客户端
// ============================================================================
//
// 「真实 LLM 模式」：设置 OPENAI_API_KEY（或 LLM_PROVIDER），
// 框架自动调用 LLM 为每个问题生成 QueryPlan。
//
// 「离线演示模式」：不设置 API Key，使用预设的 QueryPlan 通过
// compile_only() 直接编译，无需 LLM。

fn select_llm_client() -> Option<Box<dyn LlmClient>> {
    // 优先使用真实的 OpenAI 兼容 API
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY")
        && !api_key.trim().is_empty()
    {
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
        let api_base = std::env::var("OPENAI_API_BASE").ok();
        let config = LlmConfig {
            provider: LlmProvider::OpenAi,
            api_key: Some(api_key),
            api_base,
            model,
            ..LlmConfig::default()
        };
        eprintln!(
            "[INFO] 真实 LLM 模式：使用 OpenAI 兼容客户端 (model={})",
            config.model
        );
        eprintln!("       所有查询都将通过 LLM 自动生成 QueryPlan\n");
        return Some(create_llm_client(config).expect("创建 OpenAI 客户端失败"));
    }

    // 也支持其他 Provider
    if let Ok(provider) = std::env::var("LLM_PROVIDER") {
        let api_key = std::env::var(format!("{}_API_KEY", provider.to_uppercase())).ok();
        let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "default".to_owned());
        let provider_enum = match provider.to_lowercase().as_str() {
            "deepseek" => LlmProvider::DeepSeek,
            "zhipu" => LlmProvider::Zhipu,
            "anthropic" => LlmProvider::Anthropic,
            "vllm" => LlmProvider::Vllm,
            "ollama" => LlmProvider::Ollama,
            _ => LlmProvider::OpenAi,
        };
        let api_base = match provider.to_lowercase().as_str() {
            "ollama" => std::env::var("OLLAMA_BASE_URL").ok(),
            "vllm" => std::env::var("VLLM_API_BASE").ok(),
            _ => None,
        };
        // Ollama 的 Qwen 3.5/3.6 等模型不支持严格 JSON Schema 的 format 参数，
        // 需关闭严格模式，回退到 format = "json"（宽松模式）。
        let extra = if provider.to_lowercase().as_str() == "ollama" {
            [("strict_json_schema".to_owned(), serde_json::json!(false))]
                .into_iter()
                .collect()
        } else {
            std::collections::HashMap::new()
        };
        let config = LlmConfig {
            provider: provider_enum,
            api_key,
            api_base,
            model,
            extra,
            ..LlmConfig::default()
        };
        eprintln!("[INFO] 真实 LLM 模式：使用 {provider} 客户端\n");
        return Some(create_llm_client(config).expect("创建 LLM 客户端失败"));
    }

    // 未设置 API Key → 离线演示模式
    eprintln!("[INFO] 离线演示模式：未检测到 API Key");
    eprintln!("       使用预设的 QueryPlan 通过 compile_only() 直接编译。");
    eprintln!("       设置 OPENAI_API_KEY 即可切换到真实 LLM 模式，无需手动构建 QueryPlan。\n");
    None
}

// ============================================================================
// 3. 预设 QueryPlan（仅离线模式需要）
// ============================================================================
//
// 在真实 LLM 模式下，QueryPlan 由 LLM 自动生成，完全不需要这段代码。
// 离线演示模式使用 compile_only() 直接编译这些预设的 Plan。

const QUESTIONS: [&str; 22] = [
    "列出总金额超过150的已完成订单，显示订单号、客户名和总金额，按金额从高到低排序，最多10条",
    "查询状态为已完成或已发货的订单，显示订单号、金额、状态和客户名",
    "哪些商品从未被购买过？",
    "每种产品卖了多少件？",
    "统计每个客户的订单数，只显示订单数超过2的客户，按订单数从高到低排序",
    "查询总金额在100到600之间的订单，显示订单号、金额和状态，按金额从小到大排序",
    "查找邮箱以example.com结尾的用户，显示用户ID、姓名和邮箱",
    "查询下过超过200元订单的用户，显示用户ID和姓名",
    "使用CTE找出每个产品的总销售额，显示产品名和销售额，按销售额从高到低排序",
    "查询订单详情：订单号、客户名、产品名、购买数量、单价和小计，按订单号排序",
    "哪些商品从未被购买过？（使用 NOT EXISTS 实现）",
    "查询所有用户及其订单信息（包括从未下单的用户），展示用户ID、姓名、订单号和金额",
    "生成所有用户和所有产品的组合列表，展示用户名和产品名",
    "查询所有员工及其直属上级的姓名和部门（表自连接）",
    "按月统计订单数量和总金额，按月份从新到旧排序",
    "查询每个订单包含的商品名称列表（逗号分隔）和总件数",
    "统计每个商品有多少不同客户购买过以及总销量",
    "查询状态不是已完成且金额大于100的订单（NOT + AND 复杂条件）",
    "按金额区间给订单打标：高/中/低（CASE WHEN 条件分支）",
    "查询有哪些不同的客户下过单（SELECT DISTINCT 去重）",
    "每个商品分类下销量最高的商品（ROW_NUMBER 窗口函数 TopN）",
    "合并1月和2月的订单数据（UNION ALL 结果集合并）",
    "递归查询组织架构树：从赵总开始向下穿透（WITH RECURSIVE）",
];

/// 返回所有预设的 QueryPlan（离线模式使用）。
fn build_all_plans() -> Vec<QueryPlan> {
    vec![
        build_demo_plan(),
        build_in_predicate_plan(),
        build_is_null_plan(),
        build_aggregate_plan(),
        build_having_plan(),
        build_between_plan(),
        build_like_plan(),
        build_subquery_in_plan(),
        build_cte_plan(),
        build_multi_join_plan(),
        build_not_exists_plan(),
        build_full_outer_join_plan(),
        build_cross_join_plan(),
        build_self_join_plan(),
        build_date_trunc_plan(),
        build_string_agg_plan(),
        build_distinct_count_plan(),
        build_complex_not_plan(),
        build_case_when_plan(),
        build_select_distinct_plan(),
        build_window_function_plan(),
        build_union_all_plan(),
        build_recursive_cte_plan(),
    ]
}

/// Plan 1: 基础查询 —— 已完成的订单（总金额 > 150）
fn build_demo_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        r#where: Some(Predicate::And {
            left: Box::new(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "status".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: serde_json::json!("completed"),
                    data_type: DataType::String,
                },
            }),
            right: Box::new(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "total".to_owned(),
                },
                op: ComparisonOperator::Gt,
                right: Expression::Literal {
                    value: serde_json::json!(150),
                    data_type: DataType::Int,
                },
            }),
        }),
        group_by: None,
        having: None,
        distinct: false,
            distinct_on: None,order_by: Some(vec![vlorql_core::schema::OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            descending: true,
        }]),
        limit: Some(10),
        offset: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        ctes: None,
    }
}

/// Plan 2: IN 谓词 —— 查询状态为 completed 或 shipped 的订单
///
/// 对应的 SQL:
/// ```sql
/// SELECT "orders"."id", "orders"."total", "orders"."status", "users"."name"
/// FROM "orders"
/// INNER JOIN "users" ON "orders"."user_id" = "users"."id"
/// WHERE "orders"."status" IN ('completed', 'shipped')
/// ORDER BY "orders"."created_at" DESC
/// ```
fn build_in_predicate_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "users".to_owned(),
                alias: Some("u".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        r#where: Some(Predicate::In {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
            },
            target: InTarget::Values(vec![
                Expression::Literal {
                    value: serde_json::json!("completed"),
                    data_type: DataType::String,
                },
                Expression::Literal {
                    value: serde_json::json!("shipped"),
                    data_type: DataType::String,
                },
            ]),
        }),
        group_by: None,
        having: None,
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "created_at".to_owned(),
            },
            descending: true,
        }]),
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 3: IS NULL + LEFT JOIN —— 查找从未被购买过的商品
///
/// 对应的 SQL:
/// ```sql
/// SELECT "products"."id", "products"."name", "products"."price"
/// FROM "products"
/// LEFT JOIN "order_items" ON "products"."id" = "order_items"."product_id"
/// WHERE "order_items"."id" IS NULL
/// ```
fn build_is_null_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "price".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "order_items".to_owned(),
                alias: Some("oi".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("products".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("order_items".to_owned()),
                    column: "product_id".to_owned(),
                },
            },
        }]),
        r#where: Some(Predicate::IsNull {
            expr: Expression::ColumnRef {
                table: Some("order_items".to_owned()),
                column: "id".to_owned(),
            },
        }),
        group_by: None,
        having: None,
        distinct: false,
            distinct_on: None,order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 4: GROUP BY + 聚合函数 —— 每种产品的累计销售量
///
/// 对应的 SQL:
/// ```sql
/// SELECT "products"."name", SUM("order_items"."quantity") AS "total_sold"
/// FROM "products"
/// INNER JOIN "order_items" ON "products"."id" = "order_items"."product_id"
/// GROUP BY "products"."name"
/// ```
fn build_aggregate_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "quantity".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("total_sold".to_owned()),
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "order_items".to_owned(),
                alias: Some("oi".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("products".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("order_items".to_owned()),
                    column: "product_id".to_owned(),
                },
            },
        }]),
        r#where: None,
        group_by: Some(vec![Expression::ColumnRef {
            table: Some("products".to_owned()),
            column: "name".to_owned(),
        }]),
        having: None,
        distinct: false,
            distinct_on: None,order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 5: HAVING + COUNT + GROUP BY + JOIN —— 统计每个客户的订单数，仅显示超过 2 个的
///
/// 对应的 SQL:
/// ```sql
/// SELECT "users"."name", COUNT("orders"."id") AS "order_count"
/// FROM "users"
/// INNER JOIN "orders" ON "users"."id" = "orders"."user_id"
/// GROUP BY "users"."id", "users"."name"
/// HAVING COUNT("orders"."id") > 2
/// ORDER BY COUNT("orders"."id") DESC
/// ```
fn build_having_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "COUNT".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("order_count".to_owned()),
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("u".to_owned()),
        },
        r#where: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: Some("o".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                },
            },
        }]),
        group_by: Some(vec![
            Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
            },
        ]),
        having: Some(Predicate::Comparison {
            left: Expression::FunctionCall {
                name: "COUNT".to_owned(),
                args: vec![Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "id".to_owned(),
                }],
                distinct: false,
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(2),
                data_type: DataType::Int,
            },
        }),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::FunctionCall {
                name: "COUNT".to_owned(),
                args: vec![Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "id".to_owned(),
                }],
                distinct: false,
            },
            descending: true,
        }]),
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 6: BETWEEN 范围查询 —— 查找金额在 100~600 之间的订单
///
/// 对应的 SQL:
/// ```sql
/// SELECT "orders"."id", "orders"."total", "orders"."status"
/// FROM "orders"
/// WHERE "orders"."total" BETWEEN 100 AND 600
/// ORDER BY "orders"."total" ASC
/// ```
fn build_between_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Between {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            low: Expression::Literal {
                value: serde_json::json!(100),
                data_type: DataType::Int,
            },
            high: Expression::Literal {
                value: serde_json::json!(600),
                data_type: DataType::Int,
            },
        }),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            descending: false,
        }]),
        joins: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 7: LIKE 模式匹配 —— 查找邮箱以 example.com 结尾的用户
///
/// 对应的 SQL:
/// ```sql
/// SELECT "users"."id", "users"."name", "users"."email"
/// FROM "users"
/// WHERE "users"."email" LIKE '%example.com'
/// ```
fn build_like_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "email".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Like {
            expr: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "email".to_owned(),
            },
            pattern: "%example.com".to_owned(),
        }),
        joins: None,
        group_by: None,
        having: None,
        distinct: false,
            distinct_on: None,order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 8: 子查询 IN —— 查询下过超过 200 元订单的用户
///
/// 对应的 SQL:
/// ```sql
/// SELECT "users"."id", "users"."name"
/// FROM "users"
/// WHERE "users"."id" IN (
///     SELECT "orders"."user_id" FROM "orders" WHERE "orders"."total" > 200
/// )
/// ```
fn build_subquery_in_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::In {
            expr: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            target: InTarget::SubQuery(Box::new(QueryPlan {
                select: vec![Projection::Column {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                    alias: None,
                }],
                from: FromClause {
                    table: "orders".to_owned(),
                    alias: None,
                },
                r#where: Some(Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "total".to_owned(),
                    },
                    op: ComparisonOperator::Gt,
                    right: Expression::Literal {
                        value: serde_json::json!(200),
                        data_type: DataType::Int,
                    },
                }),
                group_by: None,
                having: None,
                distinct: false,
            distinct_on: None,order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
            })),
        }),
        joins: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 9: CTE (WITH) + GROUP BY + 聚合 + 二元运算 —— 每个产品的总销售额
///
/// 对应的 SQL:
/// ```sql
/// WITH "product_sales" AS (
///     SELECT "products"."id", "products"."name",
///            SUM("order_items"."quantity" * "order_items"."unit_price") AS "revenue"
///     FROM "products"
///     INNER JOIN "order_items" ON "products"."id" = "order_items"."product_id"
///     GROUP BY "products"."id", "products"."name"
/// )
/// SELECT "product_sales"."name", "product_sales"."revenue"
/// FROM "product_sales"
/// ORDER BY "product_sales"."revenue" DESC
/// ```
fn build_cte_plan() -> QueryPlan {
    let cte_query = QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::BinaryOp {
                        left: Box::new(Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "quantity".to_owned(),
                        }),
                        op: BinaryOperator::Mul,
                        right: Box::new(Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "unit_price".to_owned(),
                        }),
                    }],
                    distinct: false,
                },
                alias: Some("revenue".to_owned()),
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        r#where: None,
        joins: Some(vec![JoinClause {
            join_type: JoinType::Inner,
            right_table: FromClause {
                table: "order_items".to_owned(),
                alias: Some("oi".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("products".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("order_items".to_owned()),
                    column: "product_id".to_owned(),
                },
            },
        }]),
        group_by: Some(vec![
            Expression::ColumnRef {
                table: Some("products".to_owned()),
                column: "id".to_owned(),
            },
            Expression::ColumnRef {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
            },
        ]),
        having: None,
        distinct: false,
            distinct_on: None,order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    };

    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("product_sales".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("product_sales".to_owned()),
                column: "revenue".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "product_sales".to_owned(),
            alias: None,
        },
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("product_sales".to_owned()),
                column: "revenue".to_owned(),
            },
            descending: true,
        }]),
        ctes: Some(vec![CommonTableExpression {
            name: "product_sales".to_owned(),
            query: Box::new(cte_query),, recursive: false
        }]),
        joins: None,
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
    }
}

/// Plan 10: 多表 JOIN + 二元运算 —— 订单详情（关联客户、商品、明细）
///
/// 对应的 SQL:
/// ```sql
/// SELECT "orders"."id" AS "order_id",
///        "users"."name" AS "customer_name",
///        "products"."name" AS "product_name",
///        "order_items"."quantity",
///        "order_items"."unit_price",
///        ("order_items"."quantity" * "order_items"."unit_price") AS "subtotal"
/// FROM "orders"
/// INNER JOIN "users" ON "orders"."user_id" = "users"."id"
/// INNER JOIN "order_items" ON "orders"."id" = "order_items"."order_id"
/// INNER JOIN "products" ON "order_items"."product_id" = "products"."id"
/// ORDER BY "orders"."id" ASC
/// ```
fn build_multi_join_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: Some("customer_name".to_owned()),
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: Some("product_name".to_owned()),
            },
            Projection::Column {
                table: Some("order_items".to_owned()),
                column: "quantity".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("order_items".to_owned()),
                column: "unit_price".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::BinaryOp {
                    left: Box::new(Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "quantity".to_owned(),
                    }),
                    op: BinaryOperator::Mul,
                    right: Box::new(Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "unit_price".to_owned(),
                    }),
                },
                alias: Some("subtotal".to_owned()),
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        joins: Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "users".to_owned(),
                    alias: Some("u".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "user_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            },
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "order_items".to_owned(),
                    alias: Some("oi".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "order_id".to_owned(),
                    },
                },
            },
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "products".to_owned(),
                    alias: Some("p".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "product_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("products".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            },
        ]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
            },
            descending: false,
        }]),
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 11: NOT EXISTS —— 使用 NOT EXISTS 查找从未被购买过的商品（与 Plan 3 不同实现方式）
///
/// 对应的 SQL:
/// ```sql
/// SELECT "p"."id", "p"."name", "p"."price"
/// FROM "products" AS "p"
/// WHERE NOT EXISTS (
///     SELECT 1 FROM "order_items" AS "oi" WHERE "oi"."product_id" = "p"."id"
/// )
/// ```
fn build_not_exists_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "price".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        r#where: Some(Predicate::Not {
            child: Box::new(Predicate::Exists {
                query: Box::new(QueryPlan {
                    select: vec![Projection::Expr {
                        expression: Expression::Literal {
                            value: serde_json::json!(1),
                            data_type: DataType::Int,
                        },
                        alias: None,
                    }],
                    from: FromClause {
                        table: "order_items".to_owned(),
                        alias: Some("oi".to_owned()),
                    },
                    r#where: Some(Predicate::Comparison {
                        left: Expression::ColumnRef {
                            table: Some("order_items".to_owned()),
                            column: "product_id".to_owned(),
                        },
                        op: ComparisonOperator::Eq,
                        right: Expression::ColumnRef {
                            table: Some("products".to_owned()),
                            column: "id".to_owned(),
                        },
                    }),
                    group_by: None,
                    having: None,
                    distinct: false,
            distinct_on: None,order_by: None,
                    limit: None,
                    offset: None,
                    joins: None,
                    ctes: None,
                }),
            }),
        }),
        joins: None,
        group_by: None,
        having: None,
        order_by: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 12: FULL OUTER JOIN —— 查询所有用户及其订单（包括从未下单的用户）
///
/// 对应的 SQL:
/// ```sql
/// SELECT "u"."id" AS "user_id", "u"."name" AS "user_name",
///        "o"."id" AS "order_id", "o"."total"
/// FROM "users" AS "u"
/// FULL OUTER JOIN "orders" AS "o" ON "u"."id" = "o"."user_id"
/// ORDER BY "u"."id", "o"."id"
/// ```
fn build_full_outer_join_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: Some("user_id".to_owned()),
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: Some("user_name".to_owned()),
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("u".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Full,
            right_table: FromClause {
                table: "orders".to_owned(),
                alias: Some("o".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                },
            },
        }]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![
            OrderByTerm {
                expr: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "id".to_owned(),
                },
                descending: false,
            },
            OrderByTerm {
                expr: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "id".to_owned(),
                },
                descending: false,
            },
        ]),
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 13: CROSS JOIN —— 生成所有用户和所有产品的笛卡尔积组合
///
/// 对应的 SQL:
/// ```sql
/// SELECT "u"."name" AS "user_name", "p"."name" AS "product_name"
/// FROM "users" AS "u"
/// CROSS JOIN "products" AS "p"
/// ORDER BY "u"."name", "p"."name"
/// ```
fn build_cross_join_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: Some("user_name".to_owned()),
            },
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: Some("product_name".to_owned()),
            },
        ],
        from: FromClause {
            table: "users".to_owned(),
            alias: Some("u".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Cross,
            right_table: FromClause {
                table: "products".to_owned(),
                alias: Some("p".to_owned()),
            },
            // CROSS JOIN 不需要 ON 条件
            on: Predicate::Comparison {
                left: Expression::Literal {
                    value: serde_json::json!(true),
                    data_type: DataType::Boolean,
                },
                op: ComparisonOperator::Eq,
                right: Expression::Literal {
                    value: serde_json::json!(true),
                    data_type: DataType::Boolean,
                },
            },
        }]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![
            OrderByTerm {
                expr: Expression::ColumnRef {
                    table: Some("users".to_owned()),
                    column: "name".to_owned(),
                },
                descending: false,
            },
            OrderByTerm {
                expr: Expression::ColumnRef {
                    table: Some("products".to_owned()),
                    column: "name".to_owned(),
                },
                descending: false,
            },
        ]),
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 14: 表自连接 (SELF JOIN) —— 查询员工及其直属上级的姓名和部门
///
/// 对应的 SQL:
/// ```sql
/// SELECT "e"."name" AS "employee_name", "e"."department",
///        "m"."name" AS "manager_name"
/// FROM "employees" AS "e"
/// LEFT JOIN "employees" AS "m" ON "e"."manager_id" = "m"."id"
/// ORDER BY "e"."id"
/// ```
fn build_self_join_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("e".to_owned()),
                column: "name".to_owned(),
                alias: Some("employee_name".to_owned()),
            },
            Projection::Column {
                table: Some("e".to_owned()),
                column: "department".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("m".to_owned()),
                column: "name".to_owned(),
                alias: Some("manager_name".to_owned()),
            },
        ],
        from: FromClause {
            table: "employees".to_owned(),
            alias: Some("e".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "employees".to_owned(),
                alias: Some("m".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("e".to_owned()),
                    column: "manager_id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("m".to_owned()),
                    column: "id".to_owned(),
                },
            },
        }]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("e".to_owned()),
                column: "id".to_owned(),
            },
            descending: false,
        }]),
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 15: DATE_TRUNC + GROUP BY —— 按月统计订单数量和总金额
///
/// 对应的 SQL:
/// ```sql
/// SELECT DATE_TRUNC('month', "o"."created_at") AS "month",
///        COUNT(*) AS "order_count",
///        SUM("o"."total") AS "total_amount"
/// FROM "orders" AS "o"
/// GROUP BY DATE_TRUNC('month', "o"."created_at")
/// ORDER BY "month" DESC
/// ```
fn build_date_trunc_plan() -> QueryPlan {
    let month_expr = Expression::FunctionCall {
        name: "DATE_TRUNC".to_owned(),
        args: vec![
            Expression::Literal {
                value: serde_json::json!("month"),
                data_type: DataType::String,
            },
            Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "created_at".to_owned(),
            },
        ],
        distinct: false,
    };
    QueryPlan {
        select: vec![
            Projection::Expr {
                expression: month_expr.clone(),
                alias: Some("month".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "COUNT".to_owned(),
                    args: vec![Expression::Star],
                    distinct: false,
                },
                alias: Some("order_count".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "total".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("total_amount".to_owned()),
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        group_by: Some(vec![month_expr.clone()]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: month_expr,
            descending: true,
        }]),
        r#where: None,
        joins: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 16: STRING_AGG —— 查询每个订单包含的商品名称列表和总件数
///
/// 对应的 SQL:
/// ```sql
/// SELECT "o"."id" AS "order_id",
///        STRING_AGG("p"."name", ', ') AS "products",
///        SUM("oi"."quantity") AS "total_items"
/// FROM "orders" AS "o"
/// INNER JOIN "order_items" AS "oi" ON "o"."id" = "oi"."order_id"
/// INNER JOIN "products" AS "p" ON "oi"."product_id" = "p"."id"
/// GROUP BY "o"."id"
/// ORDER BY "o"."id"
/// ```
fn build_string_agg_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "STRING_AGG".to_owned(),
                    args: vec![
                        Expression::ColumnRef {
                            table: Some("products".to_owned()),
                            column: "name".to_owned(),
                        },
                        Expression::Literal {
                            value: serde_json::json!(", "),
                            data_type: DataType::String,
                        },
                    ],
                    distinct: false,
                },
                alias: Some("products".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "quantity".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("total_items".to_owned()),
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        joins: Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "order_items".to_owned(),
                    alias: Some("oi".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "order_id".to_owned(),
                    },
                },
            },
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "products".to_owned(),
                    alias: Some("p".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "product_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("products".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            },
        ]),
        group_by: Some(vec![Expression::ColumnRef {
            table: Some("orders".to_owned()),
            column: "id".to_owned(),
        }]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
            },
            descending: false,
        }]),
        r#where: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 17: COUNT(DISTINCT) + multiple aggregates —— 统计每个商品的不同购买客户数和总销量
///
/// 对应的 SQL:
/// ```sql
/// SELECT "p"."name",
///        COUNT(DISTINCT "o"."user_id") AS "distinct_customers",
///        SUM("oi"."quantity") AS "total_sold"
/// FROM "products" AS "p"
/// INNER JOIN "order_items" AS "oi" ON "p"."id" = "oi"."product_id"
/// INNER JOIN "orders" AS "o" ON "oi"."order_id" = "o"."id"
/// GROUP BY "p"."name"
/// ORDER BY "distinct_customers" DESC
/// ```
fn build_distinct_count_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "COUNT".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "user_id".to_owned(),
                    }],
                    distinct: true,
                },
                alias: Some("distinct_customers".to_owned()),
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "quantity".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("total_sold".to_owned()),
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        joins: Some(vec![
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "order_items".to_owned(),
                    alias: Some("oi".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("products".to_owned()),
                        column: "id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "product_id".to_owned(),
                    },
                },
            },
            JoinClause {
                join_type: JoinType::Inner,
                right_table: FromClause {
                    table: "orders".to_owned(),
                    alias: Some("o".to_owned()),
                },
                on: Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "order_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "id".to_owned(),
                    },
                },
            },
        ]),
        group_by: Some(vec![Expression::ColumnRef {
            table: Some("products".to_owned()),
            column: "name".to_owned(),
        }]),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::FunctionCall {
                name: "COUNT".to_owned(),
                args: vec![Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "user_id".to_owned(),
                }],
                distinct: true,
            },
            descending: true,
        }]),
        r#where: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 18: NOT + AND —— 查询状态不是已完成且金额大于 100 的订单
///
/// 对应的 SQL:
/// ```sql
/// SELECT "o"."id", "o"."total", "o"."status"
/// FROM "orders" AS "o"
/// WHERE (NOT ("o"."status" = $1)) AND ("o"."total" > $2)
/// ORDER BY "o"."total" DESC
/// ```
fn build_complex_not_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: Some("o".to_owned()),
        },
        r#where: Some(Predicate::And {
            left: Box::new(Predicate::Not {
                child: Box::new(Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "status".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::Literal {
                        value: serde_json::json!("completed"),
                        data_type: DataType::String,
                    },
                }),
            }),
            right: Box::new(Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("orders".to_owned()),
                    column: "total".to_owned(),
                },
                op: ComparisonOperator::Gt,
                right: Expression::Literal {
                    value: serde_json::json!(100),
                    data_type: DataType::Int,
                },
            }),
        }),
        distinct: false,
            distinct_on: None,order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            descending: true,
        }]),
        joins: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
    }
}

/// Plan 19: CASE WHEN 条件分支 —— 按金额区间给订单打标
fn build_case_when_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: Some("order_id".to_owned()),
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::Case {
                    operand: None,
                    when_thens: vec![
                        WhenThen {
                            when: Expression::BinaryOp {
                                left: Box::new(Expression::ColumnRef {
                                    table: Some("orders".to_owned()),
                                    column: "total".to_owned(),
                                }),
                                op: BinaryOperator::Gte,
                                right: Box::new(Expression::Literal {
                                    value: serde_json::json!(300),
                                    data_type: DataType::Float,
                                }),
                            },
                            then: Expression::Literal {
                                value: serde_json::json!("高"),
                                data_type: DataType::String,
                            },
                        },
                        WhenThen {
                            when: Expression::BinaryOp {
                                left: Box::new(Expression::ColumnRef {
                                    table: Some("orders".to_owned()),
                                    column: "total".to_owned(),
                                }),
                                op: BinaryOperator::Gte,
                                right: Box::new(Expression::Literal {
                                    value: serde_json::json!(100),
                                    data_type: DataType::Float,
                                }),
                            },
                            then: Expression::Literal {
                                value: serde_json::json!("中"),
                                data_type: DataType::String,
                            },
                        },
                    ],
                    else_result: Some(Box::new(Expression::Literal {
                        value: serde_json::json!("低"),
                        data_type: DataType::String,
                    })),
                },
                alias: Some("level".to_owned()),
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: None,
        },
        r#where: None,
        group_by: None,
        having: None,
        distinct: false,
        distinct_on: None,
        order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            descending: true,
        }]),
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        set_operation: None,
    }
}

/// Plan 20: SELECT DISTINCT —— 查询有哪些不同的客户下过单
fn build_select_distinct_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("users".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
        ],
        distinct: true,
        distinct_on: None,
        from: FromClause {
            table: "users".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Exists {
            query: Box::new(QueryPlan {
                select: vec![Projection::Expr {
                    expression: Expression::Literal {
                        value: serde_json::json!(1),
                        data_type: DataType::Int,
                    },
                    alias: None,
                }],
                from: FromClause {
                    table: "orders".to_owned(),
                    alias: None,
                },
                r#where: Some(Predicate::Comparison {
                    left: Expression::ColumnRef {
                        table: Some("orders".to_owned()),
                        column: "user_id".to_owned(),
                    },
                    op: ComparisonOperator::Eq,
                    right: Expression::ColumnRef {
                        table: Some("users".to_owned()),
                        column: "id".to_owned(),
                    },
                }),
                group_by: None,
                having: None,
                distinct: false,
                distinct_on: None,
                order_by: None,
                limit: None,
                offset: None,
                joins: None,
                ctes: None,
                set_operation: None,
            }),
        }),
        group_by: None,
        having: None,
        order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("users".to_owned()),
                column: "id".to_owned(),
            },
            descending: false,
        }]),
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        set_operation: None,
    }
}

/// Plan 21: 窗口函数 —— ROW_NUMBER 计算每个商品被购买的次数排名
fn build_window_function_plan() -> QueryPlan {
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::FunctionCall {
                    name: "SUM".to_owned(),
                    args: vec![Expression::ColumnRef {
                        table: Some("order_items".to_owned()),
                        column: "quantity".to_owned(),
                    }],
                    distinct: false,
                },
                alias: Some("total_sold".to_owned()),
            },
            Projection::Expr {
                expression: Expression::WindowFunction {
                    name: "ROW_NUMBER".to_owned(),
                    args: vec![],
                    distinct: false,
                    over: WindowSpec {
                        partition_by: None,
                        order_by: Some(vec![OrderByTerm {
                            expr: Expression::FunctionCall {
                                name: "SUM".to_owned(),
                                args: vec![Expression::ColumnRef {
                                    table: Some("order_items".to_owned()),
                                    column: "quantity".to_owned(),
                                }],
                                distinct: false,
                            },
                            descending: true,
                        }]),
                        frame: None,
                    },
                },
                alias: Some("rank".to_owned()),
            },
        ],
        from: FromClause {
            table: "products".to_owned(),
            alias: Some("p".to_owned()),
        },
        joins: Some(vec![JoinClause {
            join_type: JoinType::Left,
            right_table: FromClause {
                table: "order_items".to_owned(),
                alias: Some("oi".to_owned()),
            },
            on: Predicate::Comparison {
                left: Expression::ColumnRef {
                    table: Some("products".to_owned()),
                    column: "id".to_owned(),
                },
                op: ComparisonOperator::Eq,
                right: Expression::ColumnRef {
                    table: Some("order_items".to_owned()),
                    column: "product_id".to_owned(),
                },
            },
        }]),
        group_by: Some(vec![
            Expression::ColumnRef {
                table: Some("products".to_owned()),
                column: "id".to_owned(),
            },
            Expression::ColumnRef {
                table: Some("products".to_owned()),
                column: "name".to_owned(),
            },
        ]),
        distinct: false,
        distinct_on: None,
        order_by: None,
        r#where: None,
        having: None,
        limit: None,
        offset: None,
        ctes: None,
        set_operation: None,
    }
}

/// Plan 22: UNION ALL —— 合并两个时间段的订单数据
fn build_union_all_plan() -> QueryPlan {
    let right = QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            op: ComparisonOperator::Lte,
            right: Expression::Literal {
                value: serde_json::json!(100),
                data_type: DataType::Float,
            },
        }),
        group_by: None,
        having: None,
        distinct: false,
        distinct_on: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        set_operation: None,
    };

    // 外层：使用 UNION ALL 连接左右两个子查询
    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("orders".to_owned()),
                column: "status".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "orders".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::Comparison {
            left: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            op: ComparisonOperator::Gt,
            right: Expression::Literal {
                value: serde_json::json!(100),
                data_type: DataType::Float,
            },
        }),
        distinct: false,
        distinct_on: None,
        order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("orders".to_owned()),
                column: "total".to_owned(),
            },
            descending: true,
        }]),
        set_operation: Some(SetOperationClause {
            operation: SetOperation::UnionAll,
            right: Box::new(right),
        }),
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
    }
}

/// Plan 23: 递归 CTE —— 组织架构树向下穿透
fn build_recursive_cte_plan() -> QueryPlan {
    let cte_query = QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("employees".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("employees".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("employees".to_owned()),
                column: "manager_id".to_owned(),
                alias: None,
            },
            Projection::Expr {
                expression: Expression::Literal {
                    value: serde_json::json!(0),
                    data_type: DataType::Int,
                },
                alias: Some("level".to_owned()),
            },
        ],
        from: FromClause {
            table: "employees".to_owned(),
            alias: None,
        },
        r#where: Some(Predicate::IsNull {
            expr: Expression::ColumnRef {
                table: Some("employees".to_owned()),
                column: "manager_id".to_owned(),
            },
        }),
        group_by: None,
        having: None,
        distinct: false,
        distinct_on: None,
        order_by: None,
        limit: None,
        offset: None,
        joins: None,
        ctes: None,
        set_operation: Some(SetOperationClause {
            operation: SetOperation::UnionAll,
            right: Box::new(QueryPlan {
                select: vec![
                    Projection::Column {
                        table: Some("emp".to_owned()),
                        column: "id".to_owned(),
                        alias: None,
                    },
                    Projection::Column {
                        table: Some("emp".to_owned()),
                        column: "name".to_owned(),
                        alias: None,
                    },
                    Projection::Column {
                        table: Some("emp".to_owned()),
                        column: "manager_id".to_owned(),
                        alias: None,
                    },
                    Projection::Expr {
                        expression: Expression::BinaryOp {
                            left: Box::new(Expression::ColumnRef {
                                table: Some("org_tree".to_owned()),
                                column: "level".to_owned(),
                            }),
                            op: BinaryOperator::Add,
                            right: Box::new(Expression::Literal {
                                value: serde_json::json!(1),
                                data_type: DataType::Int,
                            }),
                        },
                        alias: Some("level".to_owned()),
                    },
                ],
                from: FromClause {
                    table: "employees".to_owned(),
                    alias: Some("emp".to_owned()),
                },
                joins: Some(vec![JoinClause {
                    join_type: JoinType::Inner,
                    right_table: FromClause {
                        table: "org_tree".to_owned(),
                        alias: None,
                    },
                    on: Predicate::Comparison {
                        left: Expression::ColumnRef {
                            table: Some("emp".to_owned()),
                            column: "manager_id".to_owned(),
                        },
                        op: ComparisonOperator::Eq,
                        right: Expression::ColumnRef {
                            table: Some("org_tree".to_owned()),
                            column: "id".to_owned(),
                        },
                    },
                }]),
                group_by: None,
                having: None,
                distinct: false,
                distinct_on: None,
                order_by: None,
                limit: None,
                offset: None,
                ctes: None,
                set_operation: None,
            }),
        }),
    };

    QueryPlan {
        select: vec![
            Projection::Column {
                table: Some("org_tree".to_owned()),
                column: "id".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("org_tree".to_owned()),
                column: "name".to_owned(),
                alias: None,
            },
            Projection::Column {
                table: Some("org_tree".to_owned()),
                column: "level".to_owned(),
                alias: None,
            },
        ],
        from: FromClause {
            table: "org_tree".to_owned(),
            alias: None,
        },
        distinct: false,
        distinct_on: None,
        order_by: Some(vec![OrderByTerm {
            expr: Expression::ColumnRef {
                table: Some("org_tree".to_owned()),
                column: "level".to_owned(),
            },
            descending: false,
        }]),
        ctes: Some(vec![CommonTableExpression {
            name: "org_tree".to_owned(),
            recursive: true,
            query: Box::new(cte_query),
        }]),
        joins: None,
        r#where: None,
        group_by: None,
        having: None,
        limit: None,
        offset: None,
        set_operation: None,
    }
}

// ============================================================================
// 7. 在 PostgreSQL 上执行（可选）
// ============================================================================

/// A wrapper that lets an `i64` value be serialized across PostgreSQL
/// integer types (`INT2`, `INT4`, `INT8`) and gracefully falls back to
/// `TEXT`.
///
/// PostgreSQL's extended-query protocol infers parameter types during
/// Parse.  In ambiguous positions (e.g. `ORDER BY $1` when `$1` holds a
/// bare literal, not a column reference) the server defaults to `TEXT`.
/// Without the TEXT fallback tokio-postgres raises
/// "error serializing parameter".  This wrapper handles all four.
///
/// Macro to generate a thin wrapper type implementing `ToSql` for multiple PG types.
macro_rules! make_pg_num {
    ($name:ident, $inner:ty, |$val:ident, $ty:ident, $out:ident| $body:block,
     $( $accept:ident )|+, $label:expr) => {
        #[derive(Debug)]
        struct $name($inner);

        impl tokio_postgres::types::ToSql for $name {
            fn to_sql(
                &self,
                $ty: &tokio_postgres::types::Type,
                $out: &mut bytes::BytesMut,
            ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
                use tokio_postgres::types::IsNull;
                let $val = self.0;
                $body
                Ok(IsNull::No)
            }

            fn accepts(ty: &tokio_postgres::types::Type) -> bool {
                matches!(*ty, $( tokio_postgres::types::Type::$accept )|+)
            }

            fn to_sql_checked(
                &self,
                ty: &tokio_postgres::types::Type,
                out: &mut bytes::BytesMut,
            ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
                if !Self::accepts(ty) {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{} does not accept type {}", $label, ty),
                    )));
                }
                self.to_sql(ty, out)
            }
        }
    };
}

make_pg_num!(
    PgInt,
    i64,
    |val, ty, out| {
        if *ty == tokio_postgres::types::Type::INT2 {
            out.put_i16(val as i16);
        } else if *ty == tokio_postgres::types::Type::INT4 {
            out.put_i32(val as i32);
        } else if *ty == tokio_postgres::types::Type::INT8 {
            out.put_i64(val);
        } else {
            out.extend_from_slice(val.to_string().as_bytes());
        }
    },
    INT2 | INT4 | INT8 | TEXT,
    "PgInt"
);

make_pg_num!(
    PgFloat,
    f64,
    |val, ty, out| {
        if *ty == tokio_postgres::types::Type::FLOAT4 {
            out.put_f32(val as f32);
        } else if *ty == tokio_postgres::types::Type::FLOAT8 {
            out.put_f64(val);
        } else {
            out.extend_from_slice(val.to_string().as_bytes());
        }
    },
    FLOAT4 | FLOAT8 | TEXT,
    "PgFloat"
);

/// Convert a `Parameter` to a boxed `ToSql` value for tokio-postgres.
fn to_pg_param(p: &vlorql::Parameter) -> Box<dyn tokio_postgres::types::ToSql + Sync> {
    match &p.value {
        serde_json::Value::Number(n) => match p.data_type {
            DataType::Float => Box::new(PgFloat(n.as_f64().unwrap_or(0.0))),
            DataType::Int => Box::new(PgInt(n.as_i64().unwrap_or(0))),
            _ => {
                if let Some(f) = n.as_f64() {
                    Box::new(PgFloat(f))
                } else {
                    Box::new(PgInt(n.as_i64().unwrap_or(0)))
                }
            }
        },
        serde_json::Value::String(s) => Box::new(s.clone()),
        serde_json::Value::Bool(b) => Box::new(*b),
        _ => panic!("不支持的参数类型: {:?} (value: {:?})", p.data_type, p.value),
    }
}

/// Print query results in a simple table format.
fn print_results(qidx: usize, rows: &[tokio_postgres::Row]) {
    println!("\n========== 查询 {} 结果 ==========", qidx + 1);
    println!("返回 {} 行数据:", rows.len());
    println!();

    if rows.is_empty() {
        return;
    }

    let columns = rows[0].columns();
    let col_names: Vec<&str> = columns.iter().map(|c| c.name()).collect();
    println!("  │ {} │", col_names.join(" │ "));
    println!(
        "  ├{}┤",
        col_names
            .iter()
            .map(|_| "───────")
            .collect::<Vec<_>>()
            .join("─┼─")
    );

    for row in rows {
        let values: Vec<String> = (0..row.len())
            .map(|i| {
                // PG 二进制协议不支持隐式类型转换，需逐类型尝试
                if let Ok(v) = row.try_get::<_, i32>(i) {
                    v.to_string()
                } else if let Ok(v) = row.try_get::<_, i64>(i) {
                    v.to_string()
                } else if let Ok(v) = row.try_get::<_, f64>(i) {
                    v.to_string()
                } else if let Ok(v) = row.try_get::<_, String>(i) {
                    v
                } else {
                    "NULL".to_string()
                }
            })
            .collect();
        println!("  │ {} │", values.join(" │ "));
    }
    println!("===============================");
}

async fn execute_on_postgres(queries: &[vlorql::CompiledQuery]) -> Result<(), Box<dyn Error>> {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("[SKIP] 未设置 DATABASE_URL，跳过 PostgreSQL 执行");
            eprintln!(
                "       设置示例: DATABASE_URL=\"host=localhost user=postgres dbname=test_db\""
            );
            return Ok(());
        }
    };

    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerify))
            .with_no_client_auth(),
    );
    let (client, connection) = tokio_postgres::connect(&database_url, tls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("[ERROR] 数据库连接异常: {e}");
        }
    });
    eprintln!("[OK] 已连接到 PostgreSQL");

    // 确保测试表存在
    client
        .batch_execute(
            "
        CREATE TABLE IF NOT EXISTS users (
            id SERIAL PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT NOT NULL,
            created_at TIMESTAMPTZ DEFAULT NOW()
        );
        CREATE TABLE IF NOT EXISTS orders (
            id SERIAL PRIMARY KEY,
            user_id INT NOT NULL REFERENCES users(id),
            total DOUBLE PRECISION NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at TIMESTAMPTZ DEFAULT NOW()
        );
        CREATE TABLE IF NOT EXISTS products (
            id SERIAL PRIMARY KEY,
            name TEXT NOT NULL,
            price DOUBLE PRECISION NOT NULL
        );
        CREATE TABLE IF NOT EXISTS order_items (
            id SERIAL PRIMARY KEY,
            order_id INT NOT NULL REFERENCES orders(id),
            product_id INT NOT NULL REFERENCES products(id),
            quantity INT NOT NULL,
            unit_price DOUBLE PRECISION NOT NULL
        );
        CREATE TABLE IF NOT EXISTS employees (
            id SERIAL PRIMARY KEY,
            name TEXT NOT NULL,
            manager_id INT REFERENCES employees(id),
            department TEXT NOT NULL,
            salary DOUBLE PRECISION NOT NULL
        );
        ",
        )
        .await?;
    eprintln!("[OK] 测试表已就绪");

    // 插入测试数据（清空旧数据后重新插入，确保与当前 Schema 同步）
    eprintln!("[INFO] 刷新测试数据（清空后重建）...");
    client
        .batch_execute(
            "
        TRUNCATE TABLE order_items, orders, products, users, employees RESTART IDENTITY CASCADE;
        INSERT INTO users (name, email) VALUES
            ('张三', 'zhangsan@example.com'),
            ('李四', 'lisi@example.com'),
            ('王五', 'wangwu@example.com'),
            ('赵六', 'zhaoliu@example.com');
        INSERT INTO orders (user_id, total, status, created_at) VALUES
            (1, 199.99, 'completed', '2024-01-15 10:30:00+08'),
            (1, 89.50, 'completed', '2024-01-20 14:00:00+08'),
            (2, 299.00, 'completed', '2024-02-10 09:00:00+08'),
            (2, 45.00, 'pending', '2024-02-15 11:00:00+08'),
            (3, 599.99, 'completed', '2024-03-05 16:00:00+08'),
            (3, 120.00, 'shipped', '2024-03-10 10:00:00+08'),
            (1, 55.00, 'cancelled', '2024-03-20 13:00:00+08');
        INSERT INTO products (name, price) VALUES
            ('无线鼠标', 99.99),
            ('机械键盘', 299.00),
            ('4K显示器', 1999.00),
            ('USB Hub', 49.99);
        INSERT INTO order_items (order_id, product_id, quantity, unit_price) VALUES
            (1, 1, 2, 99.99),
            (2, 2, 1, 89.50),
            (3, 2, 1, 299.00),
            (5, 3, 1, 599.99);
        INSERT INTO employees (name, manager_id, department, salary) VALUES
            ('赵总', NULL, '管理部', 50000),
            ('钱经理', 1, '技术部', 30000),
            ('孙经理', 1, '市场部', 28000),
            ('小李', 2, '技术部', 15000),
            ('小张', 2, '技术部', 12000),
            ('小王', 3, '市场部', 13000);
        ",
        )
        .await?;
    eprintln!("[OK] 测试数据已插入");

    // 依次执行所有编译后的参数化 SQL
    for (qidx, compiled) in queries.iter().enumerate() {
        eprintln!("\n═══════════════════════════════════════════════");
        eprintln!("  查询 {} / {}", qidx + 1, queries.len());
        eprintln!("═══════════════════════════════════════════════");
        eprintln!("[EXEC] SQL: {}", compiled.sql);
        if compiled.parameters.is_empty() {
            eprintln!("[EXEC] 参数: (无)");
        } else {
            eprintln!("[EXEC] 参数:");
            for (i, param) in compiled.parameters.iter().enumerate() {
                eprintln!(
                    "       ${}: {} (类型: {:?})",
                    i + 1,
                    param.value,
                    param.data_type
                );
            }
        }

        // 将参数转换为 tokio-postgres 可接受的类型
        let param_values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> = compiled
            .parameters
            .iter()
            .map(|p| to_pg_param(p))
            .collect::<Vec<_>>();

        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            param_values.iter().map(|v| v.as_ref()).collect();

        match client.query(&compiled.sql, &params).await {
            Ok(rows) => print_results(qidx, &rows),
            Err(e) => {
                eprintln!("[ERROR] 查询 {} 执行失败: {}", qidx + 1, e);
            }
        }
    }

    Ok(())
}

// ============================================================================
// 5. 主流程
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("═══════════════════════════════════════════════════════");
    println!("  VlorQl 端到端示例：从自然语言 → 参数化 SQL → PG 执行");
    println!("═══════════════════════════════════════════════════════\n");

    // ──────────────────────────────────────────────────────────────
    // A. 查看系统提示词（LLM 收到的内容，供调试参考）
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 A: 查看系统提示词（会被发送给 LLM）───\n");
    let schema = build_schema();
    let prompt_builder = PromptBuilder::new(
        Arc::clone(&schema),
        vlorql_core::schema::DialectProfile::builder()
            .dialect(SqlDialect::Postgres)
            .supports_cte(true)
            .allowed_join_types(vec![
                JoinType::Inner,
                JoinType::Left,
                JoinType::Full,
                JoinType::Cross,
            ])
            .allowed_functions(vec![
                "count".to_owned(),
                "sum".to_owned(),
                "avg".to_owned(),
                "min".to_owned(),
                "max".to_owned(),
                "date_trunc".to_owned(),
                "string_agg".to_owned(),
            ])
            .build()?,
        PolicyConfig::default(),
    );
    let system_prompt = prompt_builder.build_system_prompt("列出总金额超过150的已完成订单");
    // 提示词很长，只显示开头和结尾
    let preview: String = system_prompt
        .chars()
        .take(500)
        .chain("...（省略）...".chars())
        .collect();
    println!("{preview}");
    println!();

    // ──────────────────────────────────────────────────────────────
    // B. 构建 VlorQl Facade
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 B: 构建 VlorQl Facade ───\n");
    let llm_client = select_llm_client();
    let is_llm_mode = llm_client.is_some();
    let mut builder = VlorQl::builder()
        .with_schema(Arc::clone(&schema))
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_max_retries(3);
    if let Some(client) = llm_client {
        builder = builder.with_llm_client(client);
    }
    let vlorql = builder.build()?;
    println!("[OK] VlorQl Facade 已构建\n");

    // ──────────────────────────────────────────────────────────────
    // C. 编译全部自然语言查询
    // ──────────────────────────────────────────────────────────────
    let count = QUESTIONS.len();

    if is_llm_mode {
        println!("─── 步骤 C: 使用 LLM 生成全部 {count} 条查询 ───\n");
        println!("[INFO] 每条查询都通过 VlorQl 完整流程：");
        println!("       1. 构建系统提示词（Schema + 方言 + 策略）");
        println!("       2. 调用 LLM 生成结构化 QueryPlan");
        println!("       3. 验证 QueryPlan（Schema → 策略 → 类型 → 方言）");
        println!("       4. 编译为参数化 SQL");
        println!("       5. 如果验证失败，自动带错误信息重试 LLM");
        println!();
    } else {
        println!("─── 步骤 C: 使用预设 QueryPlan 编译全部 {count} 条查询 ───\n");
        println!("[INFO] 离线模式：使用 compile_only() 直接编译预设 Plan。");
        println!("       设置 OPENAI_API_KEY 即可让 LLM 动态生成每个查询。\n");
    }

    let mut all_compiled = Vec::with_capacity(count);
    if is_llm_mode {
        for (i, question) in QUESTIONS.iter().enumerate() {
            println!("[{}/{}] 查询: \"{}\"", i + 1, count, question);
            let compiled = vlorql.query(question).await?;
            println!("[OK]\n");
            all_compiled.push(compiled);
        }
    } else {
        for plan in build_all_plans() {
            let validated = ValidatedPlan(Arc::new(plan));
            let compiled = vlorql.compile_only(&validated)?;
            all_compiled.push(compiled);
        }
    }

    // ──────────────────────────────────────────────────────────────
    // D. 查看编译结果
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 D: 编译后的参数化 SQL ───\n");
    println!("方言: {:?}", SqlDialect::Postgres);
    println!();

    for (i, compiled) in all_compiled.iter().enumerate() {
        let question = QUESTIONS[i];
        println!("查询 {}: \"{}\"", i + 1, question);
        println!();
        println!("  SQL:");
        println!("  ─────────────────────────────────────────────");
        println!("  {}", compiled.sql);
        println!("  ─────────────────────────────────────────────");
        if compiled.parameters.is_empty() {
            println!("  参数: (无)");
        } else {
            println!("  参数:");
            for (j, param) in compiled.parameters.iter().enumerate() {
                println!(
                    "    ${}: value={}, type={:?}",
                    j + 1,
                    param.value,
                    param.data_type
                );
            }
        }
        println!();
    }

    // ──────────────────────────────────────────────────────────────
    // E. 在 PostgreSQL 上执行（可选）
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 E: 在 PostgreSQL 上执行全部查询 ───\n");
    execute_on_postgres(&all_compiled).await?;

    println!("\n═══════════════════════════════════════════════════════");
    println!("  示例运行完毕，共编译 {} 条查询", all_compiled.len());
    println!("═══════════════════════════════════════════════════════");

    Ok(())
}
