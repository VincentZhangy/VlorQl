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
use tokio_postgres_rustls::MakeRustlsConnect;
use rustls::ClientConfig;
use rustls::pki_types::{ServerName, CertificateDer};
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};

/// A certificate verifier that accepts any server certificate.
/// Only use in development/testing environments.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
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

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

use vlorql::{SchemaSnapshot, SqlDialect, VlorQl};
use vlorql_core::policy::PolicyConfig;
use vlorql_core::prompt::PromptBuilder;
use vlorql_core::schema::{ColumnSchema, DataType, SchemaMetadata, TableSchema};
use vlorql_core::schema::{
    ComparisonOperator, Expression, FromClause, JoinClause, JoinType, Predicate, Projection,
    QueryPlan,
};
use vlorql_llm::{create_llm_client, LlmClient, LlmConfig, LlmProvider, MockLlmClient};

// ============================================================================
// 1. 定义数据库 Schema
// ============================================================================

/// 电商数据库 Schema：users、orders、products、order_items 四张表。
fn build_schema() -> Arc<SchemaSnapshot> {
    Arc::new(SchemaSnapshot::new(
        vec![
            TableSchema {
                name: "users".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("用户唯一标识符".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("用户显示名称".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "email".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("用户邮箱地址".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "created_at".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("用户注册时间 (ISO-8601)".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("应用注册用户".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "orders".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("订单唯一标识符".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "user_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("关联到 users.id".to_owned()),
                        is_primary_key: false,
                        foreign_key: Some(vlorql_core::schema::ForeignKey {
                            foreign_table: "users".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    ColumnSchema {
                        name: "total".to_owned(),
                        data_type: DataType::Float,
                        nullable: false,
                        description: Some("订单总金额".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "status".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some(
                            "订单状态: pending/shipped/completed/cancelled".to_owned(),
                        ),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "created_at".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("下单时间 (ISO-8601)".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("客户订单记录".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "products".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("产品唯一标识符".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".to_owned(),
                        data_type: DataType::String,
                        nullable: false,
                        description: Some("产品名称".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "price".to_owned(),
                        data_type: DataType::Float,
                        nullable: false,
                        description: Some("产品单价".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("产品目录".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
            TableSchema {
                name: "order_items".to_owned(),
                columns: vec![
                    ColumnSchema {
                        name: "id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("明细唯一标识符".to_owned()),
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "order_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("关联到 orders.id".to_owned()),
                        is_primary_key: false,
                        foreign_key: Some(vlorql_core::schema::ForeignKey {
                            foreign_table: "orders".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    ColumnSchema {
                        name: "product_id".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("关联到 products.id".to_owned()),
                        is_primary_key: false,
                        foreign_key: Some(vlorql_core::schema::ForeignKey {
                            foreign_table: "products".to_owned(),
                            foreign_column: "id".to_owned(),
                        }),
                    },
                    ColumnSchema {
                        name: "quantity".to_owned(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: Some("购买数量".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "unit_price".to_owned(),
                        data_type: DataType::Float,
                        nullable: false,
                        description: Some("购买时单价".to_owned()),
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: Some("订单中的商品明细".to_owned()),
                primary_key: Some(vec!["id".to_owned()]),
            },
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
// 框架自动调用 LLM 生成 QueryPlan，您无需手动构建。
//
// 「离线演示模式」：未设置 API Key 时使用 MockLlmClient，
// 它会返回一个预设的 QueryPlan 来模拟 LLM 输出。

fn select_llm_client() -> Box<dyn LlmClient> {
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
        eprintln!("       您只需要输入自然语言，QueryPlan 由 LLM 自动生成\n");
        return create_llm_client(config).expect("创建 OpenAI 客户端失败");
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
        return create_llm_client(config).expect("创建 LLM 客户端失败");
    }

    // 未设置 API Key → 离线演示模式
    eprintln!("[INFO] 离线演示模式：未检测到 API Key，使用 MockLlmClient");
    eprintln!("       Mock 模式需要预设一个 QueryPlan 来模拟 LLM 输出。");
    eprintln!("       设置 OPENAI_API_KEY 即可切换到真实 LLM 模式，无需手动构建 QueryPlan。\n");
    Box::new(MockLlmClient::success(build_demo_plan()))
}

// ============================================================================
// 3. 离线演示模式：预设 QueryPlan（仅 Mock 模式需要）
// ============================================================================
//
// 注意：这段代码仅用于「离线演示模式」。
// 在真实 LLM 模式下，QueryPlan 由 LLM 自动生成，完全不需要这段代码。
//
// 预设 QueryPlan 对应自然语言问题：
//   "列出总金额超过150的已完成订单，显示订单号、客户名和总金额"

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
        order_by: Some(vec![vlorql_core::schema::OrderByTerm {
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

// ============================================================================
// 4. 在 PostgreSQL 上执行（可选）
// ============================================================================

async fn execute_on_postgres(compiled: &vlorql::CompiledQuery) -> Result<(), Box<dyn Error>> {
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

    let tls = MakeRustlsConnect::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth(),
    );
    let (client, connection) =
        tokio_postgres::connect(&database_url, tls).await?;
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
        ",
        )
        .await?;
    eprintln!("[OK] 测试表已就绪");

    // 插入测试数据（如果表空）
    let count: i64 = client
        .query_one("SELECT COUNT(*) FROM users", &[])
        .await?
        .get(0);
    if count == 0 {
        eprintln!("[INFO] 插入测试数据...");
        client
            .batch_execute(
                "
            INSERT INTO users (name, email) VALUES
                ('张三', 'zhangsan@example.com'),
                ('李四', 'lisi@example.com'),
                ('王五', 'wangwu@example.com');
            INSERT INTO orders (user_id, total, status) VALUES
                (1, 199.99, 'completed'),
                (1, 89.50, 'completed'),
                (2, 299.00, 'completed'),
                (2, 45.00, 'pending'),
                (3, 599.99, 'completed'),
                (3, 120.00, 'shipped'),
                (1, 55.00, 'cancelled');
            INSERT INTO products (name, price) VALUES
                ('无线鼠标', 99.99),
                ('机械键盘', 299.00),
                ('4K显示器', 1999.00);
            INSERT INTO order_items (order_id, product_id, quantity, unit_price) VALUES
                (1, 1, 2, 99.99),
                (2, 2, 1, 89.50),
                (3, 2, 1, 299.00),
                (5, 3, 1, 599.99);
            ",
            )
            .await?;
        eprintln!("[OK] 测试数据已插入");
    }

    // 执行编译后的参数化 SQL
    eprintln!("\n[EXEC] 执行 SQL: {}", compiled.sql);
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
        .map(|p| {
            let val: Box<dyn tokio_postgres::types::ToSql + Sync> = match &p.value {
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Box::new(i) as Box<dyn tokio_postgres::types::ToSql + Sync>
                    } else if let Some(f) = n.as_f64() {
                        Box::new(f) as Box<dyn tokio_postgres::types::ToSql + Sync>
                    } else {
                        Box::new(0i64) as Box<dyn tokio_postgres::types::ToSql + Sync>
                    }
                }
                serde_json::Value::String(s) => {
                    Box::new(s.clone()) as Box<dyn tokio_postgres::types::ToSql + Sync>
                }
                serde_json::Value::Bool(b) => {
                    Box::new(*b) as Box<dyn tokio_postgres::types::ToSql + Sync>
                }
                _ => Box::new(String::new()) as Box<dyn tokio_postgres::types::ToSql + Sync>,
            };
            val
        })
        .collect::<Vec<_>>();

    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        param_values.iter().map(|v| v.as_ref()).collect();

    let rows = client.query(&compiled.sql, &params).await?;
    println!("\n========== 查询结果 ==========");
    println!("返回 {} 行数据:", rows.len());
    println!();

    if !rows.is_empty() {
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
    }

    for row in &rows {
        let values: Vec<String> = (0..row.len())
            .map(|i| {
                let val: Option<String> = row.get(i);
                val.unwrap_or_else(|| "NULL".to_owned())
            })
            .collect();
        println!("  │ {} │", values.join(" │ "));
    }
    println!("===============================");

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
            .allowed_join_types(vec![JoinType::Inner, JoinType::Left])
            .allowed_functions(vec![
                "count".to_owned(),
                "sum".to_owned(),
                "avg".to_owned(),
                "min".to_owned(),
                "max".to_owned(),
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
    let vlorql = VlorQl::builder()
        .with_schema(Arc::clone(&schema))
        .with_dialect_name("postgres")
        .with_policy(PolicyConfig::default())
        .with_llm_client(select_llm_client())
        .with_max_retries(2)
        .build()?;
    println!("[OK] VlorQl Facade 已构建\n");

    // ──────────────────────────────────────────────────────────────
    // C. 发送自然语言查询 —— 这就是您唯一需要做的事情
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 C: 发送自然语言查询 ───\n");
    let user_question =
        "列出总金额超过150的已完成订单，显示订单号、客户名和总金额，按金额从高到低排序，最多10条";
    println!("用户问题: \"{user_question}\"");
    println!();
    println!("[INFO] VlorQl 内部自动完成以下步骤：");
    println!("       1. 构建系统提示词（Schema + 方言 + 策略）");
    println!("       2. 调用 LLM 生成结构化 QueryPlan");
    println!("       3. 验证 QueryPlan（Schema → 策略 → 类型 → 方言）");
    println!("       4. 编译为参数化 SQL");
    if std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("LLM_PROVIDER").is_ok() {
        println!("       5. 如果验证失败，自动带错误信息重试 LLM");
    }
    println!();

    let compiled = vlorql.query(user_question).await?;
    println!("[OK] 查询完成\n");

    // ──────────────────────────────────────────────────────────────
    // D. 查看编译结果
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 D: 编译后的参数化 SQL ───\n");
    println!("方言: {:?}", SqlDialect::Postgres);
    println!();
    println!("SQL:");
    println!("─────────────────────────────────────────────");
    println!("{}", compiled.sql);
    println!("─────────────────────────────────────────────");
    println!();
    if compiled.parameters.is_empty() {
        println!("参数: (无)");
    } else {
        println!("参数:");
        for (i, param) in compiled.parameters.iter().enumerate() {
            println!(
                "  ${}: value={}, type={:?}",
                i + 1,
                param.value,
                param.data_type
            );
        }
    }
    println!();

    // ──────────────────────────────────────────────────────────────
    // E. 在 PostgreSQL 上执行（可选）
    // ──────────────────────────────────────────────────────────────
    println!("─── 步骤 E: 在 PostgreSQL 上执行 ───\n");
    execute_on_postgres(&compiled).await?;

    println!("\n═══════════════════════════════════════════════════════");
    println!("  示例运行完毕");
    println!("═══════════════════════════════════════════════════════");

    Ok(())
}
