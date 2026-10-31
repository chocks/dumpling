use crate::config::ResolvedConfig;
use crate::db::introspect::SchemaSnapshot;
use crate::db::value::quote_ident;
use crate::error::{DumplingError, Result};
use crate::transform::{TransformKind, TransformRegistry, check_column};
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
    /// Full SQL `WHERE` body per table: its own filter plus constraints that keep
    /// child rows from referencing parent rows that were filtered out.
    pub effective_where: IndexMap<String, Option<String>>,
    /// `WITH ...` prefix per table defining the CTEs its `effective_where`
    /// refers to (only for tables below several levels of restricted
    /// parents; see [`effective_filters`]).
    pub effective_with: IndexMap<String, Option<String>>,
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
                    fk.columns.join(","),
                    fk.referenced_table
                ));
                continue;
            }
            if !all_tables.contains(&fk.referenced_table) {
                missing_dependencies.push(format!(
                    "{table}.{} -> {} (missing)",
                    fk.columns.join(","),
                    fk.referenced_table
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

    // Validate transforms: known, well-formed, and able to work on their column.
    let mut transform_count = 0usize;
    for (table_name, table_cfg) in &resolved.config.tables {
        if !expanded.contains(table_name) {
            continue;
        }
        let info = schema.tables.get(table_name);
        for (col_name, col_cfg) in &table_cfg.columns {
            let spec = &col_cfg.transform;
            let kind = registry.kind(spec)?;
            registry.validate_spec(spec)?;
            transform_count += 1;
            let col = info.and_then(|i| i.column(col_name)).ok_or_else(|| {
                DumplingError::Validation(format!(
                    "`{table_name}.{col_name}` has a transform but the column does not exist"
                ))
            })?;
            if kind == TransformKind::Null && !col.is_nullable {
                return Err(DumplingError::Validation(format!(
                    "transform `null` on `{table_name}.{col_name}` but column is NOT NULL"
                )));
            }
            check_column(kind, table_name, col)?;
            if kind == TransformKind::Preserve {
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
        return Err(DumplingError::Validation(format!(
            "foreign-key cycle (including self-references) among: {}. \
             Dumpling loads parents before children and cannot order these tables yet; \
             exclude one of them or break the cycle in the destination",
            cycles[0].join(", ")
        )));
    }

    for t in &load_order {
        if let Some(info) = schema.tables.get(t)
            && info.page_key().is_none()
        {
            return Err(DumplingError::Validation(format!(
                "table `{t}` has no primary key or NOT NULL unique index; \
                 Dumpling needs a stable key to copy in batches"
            )));
        }
    }

    let mut selected_tables: Vec<String> = expanded.iter().cloned().collect();
    selected_tables.sort();

    let own: HashMap<&str, &str> = resolved
        .config
        .tables
        .iter()
        .filter_map(|(t, c)| c.filter().map(|f| (t.as_str(), f)))
        .collect();
    let (effective_where, effective_with) =
        effective_filters(&selected_tables, schema, &expanded, &own);

    Ok(SyncPlan {
        selected_tables,
        excluded_tables,
        load_order,
        cycles,
        missing_dependencies,
        transform_count,
        effective_where,
        effective_with,
        estimated_rows: IndexMap::new(),
        warnings,
    })
}

/// Per table: its own filter AND, for every selected parent that is itself
/// restricted, a semi-join so a child row is only copied if its parent row is.
/// Nullable FK columns are exempt when NULL (MySQL does not check them).
///
/// Embedding a parent's whole filter text in every child makes the SQL grow
/// with the number of FK *paths* (exponentially for chains of diamonds). So a
/// parent that has no restricted ancestors of its own is inlined as a
/// subquery, while one that does is defined once as a CTE
/// (`WITH dumpling_ok_<table> AS (...)`) and referenced by name, keeping the
/// size linear. The second map holds each table's `WITH` prefix.
fn effective_filters(
    tables: &[String],
    schema: &SchemaSnapshot,
    selected: &HashSet<String>,
    own: &HashMap<&str, &str>,
) -> (
    IndexMap<String, Option<String>>,
    IndexMap<String, Option<String>>,
) {
    struct Node {
        pred: Option<String>,
        /// No restricted ancestors: `pred` is just the table's own filter.
        simple: bool,
        /// CTEs `pred` depends on, parents first.
        ctes: Vec<String>,
    }
    struct Ctx<'a> {
        schema: &'a SchemaSnapshot,
        selected: &'a HashSet<String>,
        own: &'a HashMap<&'a str, &'a str>,
        memo: HashMap<String, std::rc::Rc<Node>>,
        /// Columns each CTE table must expose.
        cte_columns: IndexMap<String, Vec<String>>,
    }
    fn cte_name(table: &str) -> String {
        quote_ident(&format!("dumpling_ok_{table}"))
    }
    fn q(cols: &[String]) -> String {
        cols.iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn node(table: &str, cx: &mut Ctx) -> std::rc::Rc<Node> {
        if let Some(n) = cx.memo.get(table) {
            return n.clone();
        }
        let mut parts = Vec::new();
        let mut ctes: Vec<String> = Vec::new();
        let mut simple = true;
        if let Some(f) = cx.own.get(table) {
            parts.push(format!("({f})"));
        }
        if let Some(info) = cx.schema.tables.get(table) {
            for fk in &info.foreign_keys {
                let parent = &fk.referenced_table;
                // Cycles were rejected earlier, so this recursion terminates.
                if !cx.selected.contains(parent) {
                    continue;
                }
                let pn = node(parent, cx);
                let Some(parent_pred) = &pn.pred else {
                    continue;
                };
                simple = false;
                let source = if pn.simple {
                    format!(
                        "SELECT {} FROM {} WHERE {parent_pred}",
                        q(&fk.referenced_columns),
                        quote_ident(parent)
                    )
                } else {
                    for dep in pn.ctes.iter().chain(std::iter::once(parent)) {
                        if !ctes.contains(dep) {
                            ctes.push(dep.clone());
                        }
                    }
                    let cols = cx.cte_columns.entry(parent.clone()).or_default();
                    for c in &fk.referenced_columns {
                        if !cols.contains(c) {
                            cols.push(c.clone());
                        }
                    }
                    format!(
                        "SELECT {} FROM {}",
                        q(&fk.referenced_columns),
                        cte_name(parent)
                    )
                };
                let semi = format!("({}) IN ({source})", q(&fk.columns));
                let null_guards: Vec<String> = fk
                    .columns
                    .iter()
                    .filter(|c| info.column(c).is_some_and(|c| c.is_nullable))
                    .map(|c| format!("{} IS NULL", quote_ident(c)))
                    .collect();
                parts.push(if null_guards.is_empty() {
                    semi
                } else {
                    format!("({} OR {semi})", null_guards.join(" OR "))
                });
            }
        }
        let n = std::rc::Rc::new(Node {
            pred: (!parts.is_empty()).then(|| parts.join(" AND ")),
            simple,
            ctes,
        });
        cx.memo.insert(table.to_string(), n.clone());
        n
    }

    let mut cx = Ctx {
        schema,
        selected,
        own,
        memo: HashMap::new(),
        cte_columns: IndexMap::new(),
    };
    let nodes: Vec<_> = tables.iter().map(|t| node(t, &mut cx)).collect();
    let mut wheres = IndexMap::new();
    let mut withs = IndexMap::new();
    for (t, n) in tables.iter().zip(&nodes) {
        wheres.insert(t.clone(), n.pred.clone());
        let with = (!n.ctes.is_empty()).then(|| {
            let defs: Vec<String> = n
                .ctes
                .iter()
                .map(|c| {
                    let cn = cx.memo[c].clone();
                    format!(
                        "{} AS (SELECT {} FROM {} WHERE {})",
                        cte_name(c),
                        q(&cx.cte_columns[c]),
                        quote_ident(c),
                        cn.pred.as_deref().unwrap_or("1")
                    )
                })
                .collect();
            format!("WITH {} ", defs.join(", "))
        });
        withs.insert(t.clone(), with);
    }
    (wheres, withs)
}

/// Sanity-check a table filter. Filters are trusted configuration, not a
/// security boundary (the config author already controls the whole sync); this
/// only catches accidents such as a stray `;` or a pasted statement.
///
/// The filter is scanned lexically: quoted strings and backticked identifiers
/// are skipped, so `note = 'drop; table'` and a column named `` `update` `` are
/// fine, while whitespace (including newlines/tabs) around keywords is
/// irrelevant.
fn validate_filter_expression(table: &str, filter: &str) -> Result<()> {
    const STATEMENT_WORDS: &[&str] = &[
        "drop", "alter", "truncate", "insert", "update", "delete", "into", "union",
    ];
    let reject = |what: &str| {
        Err(DumplingError::Validation(format!(
            "filter on `{table}` must be a single expression: found {what}"
        )))
    };
    let chars: Vec<char> = filter.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' | '`' => {
                // Skip a quoted run; a doubled quote or backslash escapes.
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && c != '`' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            ';' => return reject("a semicolon"),
            '#' => return reject("a `#` comment"),
            '/' if next == Some('*') => return reject("a `/*` comment"),
            // MySQL only treats `--` as a comment when followed by whitespace
            // (or the end), so `balance - -5` and `a--5` stay valid.
            '-' if next == Some('-') && chars.get(i + 2).is_none_or(|c| c.is_whitespace()) => {
                return reject("a `--` comment");
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect::<String>().to_lowercase();
                if STATEMENT_WORDS.contains(&word.as_str()) {
                    return reject(&format!("the keyword `{word}`"));
                }
            }
            _ => i += 1,
        }
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
                if selected.contains(&fk.referenced_table) {
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

/// Exact per-table row counts, using the same filters the copy will use.
pub async fn estimate_rows(pool: &crate::db::MySqlPool, plan: &mut SyncPlan) -> Result<()> {
    for table in &plan.load_order {
        crate::db::writer::validate_ident(table)?;
        let with = plan
            .effective_with
            .get(table)
            .and_then(|w| w.as_deref())
            .unwrap_or("");
        let mut sql = format!("{with}SELECT COUNT(*) FROM {}", quote_ident(table));
        if let Some(Some(f)) = plan.effective_where.get(table) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await
            .map_err(|e| {
                DumplingError::Database(format!(
                    "counting `{table}` failed (check its where clause): {e}"
                ))
            })?;
        plan.estimated_rows.insert(table.clone(), Some(n as u64));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Connection, Defaults, DestinationMode, TableConfig};
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
                    columns: vec!["user_id".into()],
                    referenced_table: "users".into(),
                    referenced_columns: vec!["id".into()],
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
    fn filter_rejects_statements_and_comments_regardless_of_whitespace() {
        for f in [
            "id > 1; drop table users",
            "x=1\nUNION\tSELECT 1",
            "x=1\nunion\nselect 1 into outfile 'f'",
            "id > 1 -- trailing",
            "id > 1 --",
            "id > 1 /* c */",
            "id > 1 # c",
            "(DELETE)",
        ] {
            assert!(validate_filter_expression("t", f).is_err(), "{f:?}");
        }
    }

    #[test]
    fn filter_accepts_valid_expressions() {
        for f in [
            "balance - -5 > 0",
            "a--5 > 0",
            "created_at > NOW() - INTERVAL 1 DAY",
            "note = 'drop; table -- /* # x'",
            "name = 'it''s; fine'",
            r"name = 'a\'; b'",
            "`update` = 1 AND `into` IS NULL",
            "update_time > '2024-01-01' AND deleted_at IS NULL AND dropship = 1",
            "status IN ('a','b')\n  AND x = 1",
        ] {
            validate_filter_expression("t", f).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        }
    }
}
