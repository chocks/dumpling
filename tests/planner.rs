use dumpling::config::{
    ColumnConfig, Config, Connection, Defaults, DestinationMode, TableConfig, TransformSpec,
};
use dumpling::db::introspect::{ColumnInfo, ForeignKey, SchemaSnapshot, TableInfo};
use dumpling::planner::build_plan;
use dumpling::transform::TransformRegistry;
use indexmap::IndexMap;
use std::path::PathBuf;

fn base_resolved(tables: IndexMap<String, TableConfig>) -> dumpling::config::ResolvedConfig {
    let mut connections = IndexMap::new();
    connections.insert(
        "src".into(),
        Connection {
            url: "mysql://u@127.0.0.1/src".into(),
            allow_destination: false,
            production: true,
            not_production: false,
        },
    );
    connections.insert(
        "dst".into(),
        Connection {
            url: "mysql://u@127.0.0.1/dst".into(),
            allow_destination: true,
            production: false,
            not_production: false,
        },
    );
    dumpling::config::ResolvedConfig {
        config: Config {
            version: 1,
            defaults: Defaults::default(),
            connections,
            source: Some("src".into()),
            destination: Some("dst".into()),
            mode: DestinationMode::Merge,
            tables,
            exclude: vec!["audit_logs".into()],
            groups: IndexMap::new(),
            environments: IndexMap::new(),
        },
        source_name: "src".into(),
        dest_name: "dst".into(),
        source_url: "mysql://u@127.0.0.1/src".into(),
        dest_url: "mysql://u@127.0.0.1/dst".into(),
        mode: DestinationMode::Merge,
        table_filter: None,
        group: None,
        force: false,
        dry_run: true,
        config_path: PathBuf::from(".dumpling.yml"),
    }
}

fn schema() -> SchemaSnapshot {
    let mut tables = IndexMap::new();
    tables.insert(
        "users".into(),
        TableInfo {
            name: "users".into(),
            engine: Some("InnoDB".into()),
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    column_type: "bigint".into(),
                    is_nullable: false,
                    column_default: None,
                    extra: "auto_increment".into(),
                    ordinal: 1,
                },
                ColumnInfo {
                    name: "email".into(),
                    data_type: "varchar".into(),
                    column_type: "varchar(255)".into(),
                    is_nullable: false,
                    column_default: None,
                    extra: String::new(),
                    ordinal: 2,
                },
                ColumnInfo {
                    name: "password_hash".into(),
                    data_type: "varchar".into(),
                    column_type: "varchar(255)".into(),
                    is_nullable: true,
                    column_default: None,
                    extra: String::new(),
                    ordinal: 3,
                },
            ],
            primary_key: vec!["id".into()],
            unique_indexes: vec![vec!["email".into()]],
            foreign_keys: vec![],
            auto_increment: Some("id".into()),
        },
    );
    tables.insert(
        "orders".into(),
        TableInfo {
            name: "orders".into(),
            engine: Some("InnoDB".into()),
            columns: vec![ColumnInfo {
                name: "id".into(),
                data_type: "bigint".into(),
                column_type: "bigint".into(),
                is_nullable: false,
                column_default: None,
                extra: "auto_increment".into(),
                ordinal: 1,
            }],
            primary_key: vec!["id".into()],
            unique_indexes: vec![],
            foreign_keys: vec![ForeignKey {
                constraint_name: "fk".into(),
                columns: vec!["user_id".into()],
                referenced_table: "users".into(),
                referenced_columns: vec!["id".into()],
            }],
            auto_increment: Some("id".into()),
        },
    );
    tables.insert(
        "audit_logs".into(),
        TableInfo {
            name: "audit_logs".into(),
            engine: Some("InnoDB".into()),
            columns: vec![ColumnInfo {
                name: "id".into(),
                data_type: "bigint".into(),
                column_type: "bigint".into(),
                is_nullable: false,
                column_default: None,
                extra: "auto_increment".into(),
                ordinal: 1,
            }],
            primary_key: vec!["id".into()],
            unique_indexes: vec![],
            foreign_keys: vec![],
            auto_increment: Some("id".into()),
        },
    );
    SchemaSnapshot {
        database: "app".into(),
        tables,
    }
}

#[test]
fn plan_excludes_and_orders_by_fk() {
    let mut tables = IndexMap::new();
    tables.insert(
        "users".into(),
        TableConfig {
            columns: IndexMap::from([(
                "password_hash".into(),
                ColumnConfig {
                    transform: TransformSpec::named("null"),
                },
            )]),
            ..Default::default()
        },
    );
    tables.insert("orders".into(), TableConfig::default());
    let plan = build_plan(
        &base_resolved(tables),
        &schema(),
        &TransformRegistry::with_builtins(),
    )
    .unwrap();
    assert!(!plan.selected_tables.iter().any(|t| t == "audit_logs"));
    assert!(
        plan.load_order.iter().position(|t| t == "users").unwrap()
            < plan.load_order.iter().position(|t| t == "orders").unwrap()
    );
}

#[test]
fn rejects_null_on_not_null_column() {
    let mut tables = IndexMap::new();
    tables.insert(
        "users".into(),
        TableConfig {
            columns: IndexMap::from([(
                "email".into(),
                ColumnConfig {
                    transform: TransformSpec::named("null"),
                },
            )]),
            ..Default::default()
        },
    );
    let err = build_plan(
        &base_resolved(tables),
        &schema(),
        &TransformRegistry::with_builtins(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("NOT NULL"));
}

#[test]
fn parent_filter_becomes_child_semi_join() {
    let mut tables = IndexMap::new();
    tables.insert(
        "users".into(),
        TableConfig {
            r#where: Some("status = 'active'".into()),
            ..Default::default()
        },
    );
    tables.insert("orders".into(), TableConfig::default());
    let plan = build_plan(
        &base_resolved(tables),
        &schema(),
        &TransformRegistry::with_builtins(),
    )
    .unwrap();
    assert_eq!(
        plan.effective_where["users"].as_deref(),
        Some("(status = 'active')")
    );
    assert_eq!(
        plan.effective_where["orders"].as_deref(),
        Some("(`user_id`) IN (SELECT `id` FROM `users` WHERE (status = 'active'))")
    );
}

#[test]
fn unfiltered_parent_adds_no_child_constraint() {
    let mut tables = IndexMap::new();
    tables.insert("orders".into(), TableConfig::default());
    let plan = build_plan(
        &base_resolved(tables),
        &schema(),
        &TransformRegistry::with_builtins(),
    )
    .unwrap();
    assert_eq!(plan.effective_where["orders"], None);
}

#[test]
fn rejects_foreign_key_cycles() {
    let mut s = schema();
    // users -> orders makes users <-> orders a cycle.
    s.tables
        .get_mut("users")
        .unwrap()
        .foreign_keys
        .push(ForeignKey {
            constraint_name: "fk_back".into(),
            columns: vec!["order_id".into()],
            referenced_table: "orders".into(),
            referenced_columns: vec!["id".into()],
        });
    let mut tables = IndexMap::new();
    tables.insert("orders".into(), TableConfig::default());
    let err = build_plan(
        &base_resolved(tables),
        &s,
        &TransformRegistry::with_builtins(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
}

/// `levels` stacked diamonds: t0 <- (a1, b1) <- t1 <- (a2, b2) <- t2 ...
/// The number of FK paths from the last table to t0 is 2^levels.
fn diamond_chain(levels: usize) -> (SchemaSnapshot, String) {
    let table = |name: &str, parents: &[&str]| TableInfo {
        name: name.into(),
        engine: Some("InnoDB".into()),
        columns: std::iter::once("id".to_string())
            .chain(parents.iter().map(|p| format!("{p}_id")))
            .enumerate()
            .map(|(i, n)| ColumnInfo {
                name: n,
                data_type: "bigint".into(),
                column_type: "bigint".into(),
                is_nullable: false,
                column_default: None,
                extra: String::new(),
                ordinal: i as u32 + 1,
            })
            .collect(),
        primary_key: vec!["id".into()],
        unique_indexes: vec![],
        foreign_keys: parents
            .iter()
            .map(|p| ForeignKey {
                constraint_name: format!("fk_{name}_{p}"),
                columns: vec![format!("{p}_id")],
                referenced_table: (*p).into(),
                referenced_columns: vec!["id".into()],
            })
            .collect(),
        auto_increment: None,
    };
    let mut tables = IndexMap::new();
    tables.insert("t0".to_string(), table("t0", &[]));
    for l in 1..=levels {
        let prev = format!("t{}", l - 1);
        let (a, b, t) = (format!("a{l}"), format!("b{l}"), format!("t{l}"));
        tables.insert(a.clone(), table(&a, &[&prev]));
        tables.insert(b.clone(), table(&b, &[&prev]));
        tables.insert(t.clone(), table(&t, &[&a, &b]));
    }
    (
        SchemaSnapshot {
            database: "src".into(),
            tables,
        },
        format!("t{levels}"),
    )
}

#[test]
fn filter_sql_stays_linear_for_diamond_chains() {
    let size = |levels: usize| {
        let (schema, last) = diamond_chain(levels);
        let mut tables = IndexMap::new();
        tables.insert(
            "t0".to_string(),
            TableConfig {
                r#where: Some("id < 100".into()),
                ..Default::default()
            },
        );
        tables.insert(last.clone(), TableConfig::default());
        let mut resolved = base_resolved(tables);
        resolved.config.exclude.clear();
        let plan = build_plan(&resolved, &schema, &TransformRegistry::with_builtins()).unwrap();
        plan.effective_where[&last].as_ref().unwrap().len()
            + plan.effective_with[&last].as_ref().map_or(0, |w| w.len())
    };
    let (s10, s20) = (size(10), size(20));
    // Linear growth: doubling the depth roughly doubles the size. The old
    // inlined form would be ~1000x larger at 20 levels than at 10.
    assert!(
        s20 < s10 * 3,
        "10 levels: {s10} bytes, 20 levels: {s20} bytes"
    );
    assert!(s20 < 20_000, "{s20}");
}
