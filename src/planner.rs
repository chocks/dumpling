use crate::config::{ResolvedConfig, TableConfig};
use crate::db::introspect::SchemaSnapshot;
use crate::error::{DumplingError, Result};
use crate::transform::TransformRegistry;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone)]
pub struct SyncPlan {
    pub selected_tables: Vec<String>,
    pub excluded_tables: Vec<String>,
    pub load_order: Vec<String>,
    pub cycles: Vec<Vec<String>>,
    pub missing_dependencies: Vec<String>,
    pub transform_count: usize,
    pub table_filters: IndexMap<String, Option<String>>,
    pub estimated_rows: IndexMap<String, Option<u64>>,
    pub warnings: Vec<String>,
}

/// Build a copy plan from config + schema without mutating anything.
pub fn build_plan(
    resolved: &ResolvedConfig,
    schema: &SchemaSnapshot,
    registry: &TransformRegistry,
) -> Result<SyncPlan> {
    let mut warnings = Vec::new();
    let all_tables: HashSet<String> = schema.tables.keys().cloned().collect();

    let excluded: HashSet<String> = resolved.config.exclude.iter().cloned().collect();
    let mut excluded_tables: Vec<String> = excluded.iter().cloned().collect();
    excluded_tables.sort();

    let mut selected: HashSet<String> = if let Some(ref group) = resolved.group {
        let members = resolved
            .config
            .groups
            .get(group)
            .ok_or_else(|| DumplingError::Config(format!("group `{group}` not found")))?;
        members.iter().cloned().collect()
    } else if let Some(ref filter) = resolved.table_filter {
        filter.iter().cloned().collect()
    } else if !resolved.config.tables.is_empty() {
        // Explicit table rules define the working set; other tables are skipped
        // unless referenced as FK dependencies (added below).
        resolved.config.tables.keys().cloned().collect()
    } else {
        all_tables
            .iter()
            .filter(|t| !excluded.contains(*t))
            .cloned()
            .collect()
    };

    // Remove exclusions.
    selected.retain(|t| !excluded.contains(t));

    // Validate named tables exist.
    for t in selected.iter() {
        if !all_tables.contains(t) {
            return Err(DumplingError::Validation(format!(
                "table `{t}` not found in source schema `{}`",
                schema.database
            )));
        }
    }

    // Expand FK dependencies (parents required for referential integrity).
    let mut missing_dependencies = Vec::new();
    let mut expanded = selected.clone();
    let mut queue: VecDeque<String> = selected.iter().cloned().collect();
    while let Some(table) = queue.pop_front() {
        let Some(info) = schema.tables.get(&table) else {
            continue;
        };
        for fk in &info.foreign_keys {
            if excluded.contains(&fk.referenced_table) {
                missing_dependencies.push(format!(
                    "{table}.{} -> {} (excluded)",
                    fk.column_name, fk.referenced_table
                ));
                continue;
            }
            if !all_tables.contains(&fk.referenced_table) {
                missing_dependencies.push(format!(
                    "{table}.{} -> {} (missing)",
                    fk.column_name, fk.referenced_table
                ));
                continue;
            }
            if expanded.insert(fk.referenced_table.clone()) {
                queue.push_back(fk.referenced_table.clone());
                if !resolved.config.tables.contains_key(&fk.referenced_table)
                    && resolved.table_filter.is_some()
                {
                    warnings.push(format!(
                        "added dependency `{}` required by `{table}`",
                        fk.referenced_table
                    ));
                }
            }
        }
    }

    if !missing_dependencies.is_empty() {
        return Err(DumplingError::Validation(format!(
            "missing foreign-key dependencies:\n  - {}",
            missing_dependencies.join("\n  - ")
        )));
    }

    // Validate transforms against registry and nullability.
    let mut transform_count = 0usize;
    for (table_name, table_cfg) in &resolved.config.tables {
        if !expanded.contains(table_name) {
            continue;
        }
        let info = schema.tables.get(table_name);
        for (col_name, col_cfg) in &table_cfg.columns {
            registry.validate_spec(&col_cfg.transform)?;
            transform_count += 1;
            if (col_cfg.transform.name() == "null" || col_cfg.transform.name() == "NULL")
                && let Some(info) = info
                && let Some(col) = info.columns.iter().find(|c| c.name == *col_name)
                && !col.is_nullable
            {
                return Err(DumplingError::Validation(format!(
                    "transform `null` on `{table_name}.{col_name}` but column is NOT NULL"
                )));
            }
            if col_cfg.transform.name() == "preserve" {
                warnings.push(format!(
                    "`{table_name}.{col_name}` uses transform `preserve` — raw value retained"
                ));
            }
        }
    }

    // Validate filters are non-empty administrator SQL fragments (trusted).
    for (table_name, table_cfg) in &resolved.config.tables {
        if let Some(filter) = table_cfg.filter() {
            validate_filter_expression(table_name, filter)?;
        }
    }

    let (load_order, cycles) = topological_sort(&expanded, schema)?;
    if !cycles.is_empty() {
        for cycle in &cycles {
            warnings.push(format!(
                "foreign-key cycle detected: {} — load order may require deferred constraints",
                cycle.join(" -> ")
            ));
        }
    }

    // Tables without a usable key for keyset pagination.
    for t in &load_order {
        if let Some(info) = schema.tables.get(t)
            && info.primary_key.is_empty()
            && info.unique_indexes.is_empty()
        {
            return Err(DumplingError::Validation(format!(
                "table `{t}` has no primary key or unique index; \
                     Dumpling requires a stable unique key for batched copies"
            )));
        }
    }

    let mut selected_tables: Vec<String> = expanded.into_iter().collect();
    selected_tables.sort();

    let mut table_filters = IndexMap::new();
    for t in &selected_tables {
        let filter = resolved
            .config
            .tables
            .get(t)
            .and_then(TableConfig::filter)
            .map(str::to_string);
        table_filters.insert(t.clone(), filter);
    }

    Ok(SyncPlan {
        selected_tables,
        excluded_tables,
        load_order,
        cycles,
        missing_dependencies,
        transform_count,
        table_filters,
        estimated_rows: IndexMap::new(),
        warnings,
    })
}

fn validate_filter_expression(table: &str, filter: &str) -> Result<()> {
    let lower = filter.to_ascii_lowercase();
    // Defense in depth: reject obvious multi-statement / DDL injection in filters.
    // Filters are trusted admin input, but we still block footguns.
    let banned = [
        ";",
        "--",
        "/*",
        "*/",
        " drop ",
        " alter ",
        " truncate ",
        " insert ",
        " update ",
        " delete ",
        " into ",
    ];
    for b in banned {
        if lower.contains(b.trim()) && b != ";" {
            // Allow words inside identifiers carefully — use crude check.
            if lower.contains(b) {
                return Err(DumplingError::Validation(format!(
                    "filter on `{table}` contains disallowed SQL keyword/fragment `{b}`"
                )));
            }
        }
    }
    if filter.contains(';') {
        return Err(DumplingError::Validation(format!(
            "filter on `{table}` must be a single expression (no semicolons)"
        )));
    }
    Ok(())
}

fn topological_sort(
    selected: &HashSet<String>,
    schema: &SchemaSnapshot,
) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();

    for t in selected {
        indegree.entry(t.clone()).or_insert(0);
        deps.entry(t.clone()).or_default();
    }

    for t in selected {
        if let Some(info) = schema.tables.get(t) {
            for fk in &info.foreign_keys {
                if selected.contains(&fk.referenced_table) && fk.referenced_table != *t {
                    // Edge: parent -> child (parent must load first)
                    deps.entry(fk.referenced_table.clone())
                        .or_default()
                        .push(t.clone());
                    *indegree.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut queue: VecDeque<String> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    // Stable order among ready nodes.
    let mut ready: Vec<String> = queue.drain(..).collect();
    ready.sort();
    queue.extend(ready);

    let mut order = Vec::new();
    while let Some(n) = queue.pop_front() {
        order.push(n.clone());
        if let Some(children) = deps.get(&n) {
            let mut next_ready = Vec::new();
            for child in children {
                if let Some(d) = indegree.get_mut(child) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        next_ready.push(child.clone());
                    }
                }
            }
            next_ready.sort();
            queue.extend(next_ready);
        }
    }

    let mut cycles = Vec::new();
    if order.len() != selected.len() {
        let leftover: Vec<String> = indegree
            .into_iter()
            .filter(|(_, d)| *d > 0)
            .map(|(k, _)| k)
            .collect();
        cycles.push(leftover.clone());
        // Append leftover in sorted order so the plan remains usable with warnings.
        let mut rest = leftover;
        rest.sort();
        for t in rest {
            if !order.contains(&t) {
                order.push(t);
            }
        }
    }

    Ok((order, cycles))
}

pub async fn estimate_rows(pool: &crate::db::MySqlPool, plan: &mut SyncPlan) -> Result<()> {
    for table in &plan.load_order {
        let filter = plan.table_filters.get(table).and_then(|f| f.as_ref());
        let count = match filter {
            Some(f) => {
                crate::db::writer::validate_ident(table)?;
                let sql = format!(
                    "SELECT COUNT(*) FROM `{}` WHERE {}",
                    table.replace('`', "``"),
                    f
                );
                let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .fetch_one(pool)
                    .await
                    .map_err(|e| {
                        DumplingError::Database(format!(
                            "count for `{table}` failed (check where clause): {e}"
                        ))
                    })?;
                Some(n as u64)
            }
            None => {
                // Approximate via information_schema for speed; exact COUNT(*) optional later.
                let (n,): (Option<i64>,) = sqlx::query_as(
                    r#"
                    SELECT TABLE_ROWS
                    FROM information_schema.TABLES
                    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?
                    "#,
                )
                .bind(table)
                .fetch_one(pool)
                .await
                .map_err(|e| DumplingError::Database(format!("estimate `{table}` failed: {e}")))?;
                n.map(|v| v.max(0) as u64)
            }
        };
        plan.estimated_rows.insert(table.clone(), count);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Connection, Defaults, DestinationMode};
    use crate::db::introspect::{ColumnInfo, ForeignKey, TableInfo};
    use std::path::PathBuf;

    fn schema_users_orders() -> SchemaSnapshot {
        let mut tables = IndexMap::new();
        tables.insert(
            "users".into(),
            TableInfo {
                name: "users".into(),
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
        tables.insert(
            "orders".into(),
            TableInfo {
                name: "orders".into(),
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
                        name: "user_id".into(),
                        data_type: "bigint".into(),
                        column_type: "bigint".into(),
                        is_nullable: false,
                        column_default: None,
                        extra: String::new(),
                        ordinal: 2,
                    },
                ],
                primary_key: vec!["id".into()],
                unique_indexes: vec![],
                foreign_keys: vec![ForeignKey {
                    constraint_name: "fk_orders_user".into(),
                    column_name: "user_id".into(),
                    referenced_table: "users".into(),
                    referenced_column: "id".into(),
                }],
                auto_increment: Some("id".into()),
            },
        );
        SchemaSnapshot {
            database: "app".into(),
            tables,
        }
    }

    fn resolved_for(tables: IndexMap<String, TableConfig>) -> ResolvedConfig {
        let mut connections = IndexMap::new();
        connections.insert(
            "src".into(),
            Connection {
                url: "mysql://u@127.0.0.1/src".into(),
                allow_destination: false,
                production: true,
            },
        );
        connections.insert(
            "dst".into(),
            Connection {
                url: "mysql://u@127.0.0.1/dst".into(),
                allow_destination: true,
                production: false,
            },
        );
        ResolvedConfig {
            config: Config {
                version: 1,
                defaults: Defaults::default(),
                connections,
                source: Some("src".into()),
                destination: Some("dst".into()),
                mode: DestinationMode::Merge,
                tables,
                exclude: vec![],
                groups: IndexMap::new(),
                environments: IndexMap::new(),
            },
            source_name: "src".into(),
            dest_name: "dst".into(),
            source_url: "mysql://u@127.0.0.1/src".into(),
            dest_url: "mysql://u@127.0.0.1/dst".into(),
            mode: DestinationMode::Merge,
            table_filter: Some(vec!["orders".into()]),
            group: None,
            force: false,
            dry_run: true,
            config_path: PathBuf::from(".dumpling.yml"),
        }
    }

    #[test]
    fn expands_fk_dependencies() {
        let mut tables = IndexMap::new();
        tables.insert("orders".into(), TableConfig::default());
        let resolved = resolved_for(tables);
        let schema = schema_users_orders();
        let reg = TransformRegistry::with_builtins();
        let plan = build_plan(&resolved, &schema, &reg).unwrap();
        assert!(
            plan.load_order.iter().position(|t| t == "users").unwrap()
                < plan.load_order.iter().position(|t| t == "orders").unwrap()
        );
        assert!(plan.selected_tables.contains(&"users".into()));
    }

    #[test]
    fn rejects_semicolon_in_filter() {
        let err = validate_filter_expression("users", "id > 1; drop table users").unwrap_err();
        assert!(err.to_string().contains("semicolon") || err.to_string().contains("disallowed"));
    }
}
