use std::sync::Arc;
use vlorql_core::policy::PolicyConfig;
use vlorql_core::prompt::PromptBuilder;
use vlorql_core::schema::*;

#[test]
fn check_prompt_size() {
    let schema = Arc::new(SchemaSnapshot::new(
        vec![
            TableSchema {
                name: "users".into(),
                columns: vec![
                    ColumnSchema {
                        name: "id".into(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".into(),
                        data_type: DataType::String,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "email".into(),
                        data_type: DataType::String,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "created_at".into(),
                        data_type: DataType::String,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: None,
                primary_key: Some(vec!["id".into()]),
            },
            TableSchema {
                name: "organizations".into(),
                columns: vec![
                    ColumnSchema {
                        name: "id".into(),
                        data_type: DataType::Int,
                        nullable: false,
                        description: None,
                        is_primary_key: true,
                        foreign_key: None,
                    },
                    ColumnSchema {
                        name: "name".into(),
                        data_type: DataType::String,
                        nullable: false,
                        description: None,
                        is_primary_key: false,
                        foreign_key: None,
                    },
                ],
                description: None,
                primary_key: Some(vec!["id".into()]),
            },
        ],
        SchemaMetadata::default(),
    ));

    let builder = PromptBuilder::new(schema, DialectProfile::default(), PolicyConfig::default());
    let prompt = builder.build_system_prompt("Show users and their organizations");
    println!("Prompt size: {} chars", prompt.chars().count());
    println!("Prompt size: {} bytes", prompt.len());
    assert!(prompt.chars().count() < 12_000);
}
