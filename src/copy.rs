//! Streaming MySQL → MySQL copy engine.
//!
//! Privacy boundary:
//! - Transforms `null`, `constant`, and `preserve` are applied in the source
//!   SELECT list when possible so raw values never enter the Rust process.
//! - All other transforms run client-side: raw values are held only in memory
//!   for the current batch and never written to disk or logged.

use crate::config::{DestinationMode, ResolvedConfig};
use crate::db::MySqlPool;
use crate::db::introspect::{SchemaSnapshot, TableInfo};
use crate::db::writer::{self, build_upsert_sql};
use crate::error::{DumplingError, Result};
use crate::planner::SyncPlan;
use crate::progress::Reporter;
use crate::transform::{TransformContext, TransformRegistry, TransformedValue};
use sqlx::mysql::MySqlRow;
use sqlx::{AssertSqlSafe, Column, Row, TypeInfo, ValueRef};
use std::time::Instant;

pub struct CopyEngine<'a> {
    pub resolved: &'a ResolvedConfig,
    pub source: &'a MySqlPool,
    pub dest: &'a MySqlPool,
    pub source_schema: &'a SchemaSnapshot,
    pub plan: &'a SyncPlan,
    pub registry: &'a TransformRegistry,
    pub reporter: &'a Reporter,
}

impl CopyEngine<'_> {
    pub async fn run(&self) -> Result<()> {
        if self.resolved.dry_run {
            return Ok(());
        }

        let mut ctx = TransformContext::new(
            self.resolved.config.defaults.deterministic,
            self.resolved
                .config
                .defaults
                .transform_secret
                .as_deref()
                .unwrap_or("dumpling-dev-insecure-secret-change-me"),
        );

        if self.resolved.config.defaults.transform_secret.is_none()
            && self.resolved.config.defaults.anonymize
            && self.plan.transform_count > 0
        {
            tracing::warn!(
                "DUMPLING_TRANSFORM_SECRET not set; using insecure built-in secret — \
                 set a strong secret for deterministic anonymization"
            );
        }

        self.reporter.copying_start();

        // Replace mode: clear destination tables in reverse dependency order.
        if matches!(self.resolved.mode, DestinationMode::Replace) {
            for table in self.plan.load_order.iter().rev() {
                writer::delete_all(self.dest, table).await?;
            }
        }

        for table_name in &self.plan.load_order {
            let started = Instant::now();
            let rows = self.copy_table(table_name, &mut ctx).await?;
            self.reporter.table_done(table_name, rows);
            tracing::info!(
                table = %table_name,
                rows,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "table copy finished"
            );
        }

        Ok(())
    }

    async fn copy_table(&self, table_name: &str, ctx: &mut TransformContext) -> Result<u64> {
        let info = self.source_schema.tables.get(table_name).ok_or_else(|| {
            DumplingError::Validation(format!("missing table metadata for `{table_name}`"))
        })?;

        let key_cols = if !info.primary_key.is_empty() {
            info.primary_key.clone()
        } else {
            info.unique_indexes.first().cloned().ok_or_else(|| {
                DumplingError::Validation(format!("table `{table_name}` has no primary/unique key"))
            })?
        };

        let table_cfg = self.resolved.config.tables.get(table_name);
        let batch_size = self.resolved.config.defaults.batch_size;
        let anonymize = self.resolved.config.defaults.anonymize;

        let column_names: Vec<String> = info.columns.iter().map(|c| c.name.clone()).collect();
        let select_list = self.build_select_list(info, table_cfg, anonymize, &mut ctx.scope)?;

        let filter = table_cfg.and_then(|c| c.filter());
        let order = key_cols
            .iter()
            .map(|c| format!("`{}`", c.replace('`', "``")))
            .collect::<Vec<_>>()
            .join(", ");

        let mut total = 0u64;
        let mut last_key: Option<Vec<KeyPart>> = None;

        loop {
            let (sql, bind_keys) = self.build_page_query(
                table_name,
                &select_list,
                filter,
                &key_cols,
                &order,
                last_key.as_ref(),
                batch_size,
            )?;

            // Table/column idents validated; filter is trusted admin config.
            let mut query = sqlx::query(AssertSqlSafe(sql));
            if let Some(keys) = &bind_keys {
                for part in keys {
                    query = match part {
                        KeyPart::I64(v) => query.bind(*v),
                        KeyPart::U64(v) => query.bind(*v),
                        KeyPart::Text(v) => query.bind(v.as_str()),
                        KeyPart::Bytes(v) => query.bind(v.as_slice()),
                    };
                }
            }

            let rows = query.fetch_all(self.source).await.map_err(|e| {
                DumplingError::Database(format!("select from `{table_name}` failed: {e}"))
            })?;

            if rows.is_empty() {
                break;
            }

            let batch_len = rows.len();
            let mut transformed_rows: Vec<Vec<TransformedValue>> = Vec::with_capacity(batch_len);

            for row in &rows {
                let mut out = Vec::with_capacity(column_names.len());
                for (idx, col_name) in column_names.iter().enumerate() {
                    let transform = table_cfg
                        .and_then(|c| c.columns.get(col_name))
                        .map(|c| &c.transform);

                    let value = if let Some(spec) = transform {
                        if anonymize && !self.registry.supports_source_side(spec.name()) {
                            ctx.scope = format!("{table_name}.{col_name}");
                            let raw = row_value_as_text(row, idx)?;
                            self.registry.apply(spec, raw.as_deref(), ctx)?
                        } else if self.registry.supports_source_side(spec.name())
                            && (spec.name() == "null" || spec.name() == "NULL")
                        {
                            TransformedValue::Null
                        } else if spec.name() == "constant" {
                            // Constant already applied in SQL; read returned value.
                            row_to_transformed(row, idx)?
                        } else {
                            row_to_transformed(row, idx)?
                        }
                    } else {
                        row_to_transformed(row, idx)?
                    };
                    out.push(value);
                }
                transformed_rows.push(out);
            }

            // Capture keyset cursor from last raw row before drop.
            last_key = Some(extract_key(rows.last().unwrap(), info, &key_cols)?);

            self.insert_batch(info, &column_names, &transformed_rows)
                .await?;
            total += batch_len as u64;

            if batch_len < batch_size {
                break;
            }
        }

        Ok(total)
    }

    fn build_select_list(
        &self,
        info: &TableInfo,
        table_cfg: Option<&crate::config::TableConfig>,
        anonymize: bool,
        _scope: &mut String,
    ) -> Result<String> {
        let mut parts = Vec::new();
        for col in &info.columns {
            writer::validate_ident(&col.name)?;
            let quoted = format!("`{}`", col.name.replace('`', "``"));
            let transform = table_cfg.and_then(|c| c.columns.get(&col.name));
            if anonymize && let Some(cfg) = transform {
                let name = cfg.transform.name();
                if name == "null" || name == "NULL" {
                    // Source-side nulling — raw value never leaves MySQL.
                    parts.push(format!("NULL AS {quoted}"));
                    continue;
                }
                if name == "constant"
                    && let Some(v) = cfg.transform.constant_value()
                {
                    parts.push(format!("{} AS {quoted}", yaml_sql_literal(v)?));
                    continue;
                }
                if name == "preserve" {
                    parts.push(quoted);
                    continue;
                }
            }
            // Client-side transforms: select raw column (exposure boundary).
            parts.push(quoted);
        }
        Ok(parts.join(", "))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_page_query(
        &self,
        table: &str,
        select_list: &str,
        filter: Option<&str>,
        key_cols: &[String],
        order: &str,
        last_key: Option<&Vec<KeyPart>>,
        batch_size: usize,
    ) -> Result<(String, Option<Vec<KeyPart>>)> {
        writer::validate_ident(table)?;
        let mut where_parts = Vec::new();
        if let Some(f) = filter {
            where_parts.push(format!("({f})"));
        }

        let bind_keys = if let Some(keys) = last_key {
            // Keyset: (k1, k2, ...) > (?, ?, ...)
            let tuple = key_cols
                .iter()
                .map(|c| format!("`{}`", c.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            let placeholders = key_cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            where_parts.push(format!("({tuple}) > ({placeholders})"));
            Some(keys.clone())
        } else {
            None
        };

        let where_sql = if where_parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_parts.join(" AND "))
        };

        let sql = format!(
            "SELECT {select_list} FROM `{}`{where_sql} ORDER BY {order} LIMIT {batch_size}",
            table.replace('`', "``")
        );
        Ok((sql, bind_keys))
    }

    async fn insert_batch(
        &self,
        info: &TableInfo,
        columns: &[String],
        rows: &[Vec<TransformedValue>],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let col_refs: Vec<&str> = columns.iter().map(|s| s.as_str()).collect();
        let sql = build_upsert_sql(info, &col_refs, rows.len())?;

        // Flatten owned values for binding — sqlx needs 'static-ish borrows;
        // we bind by cloning into the query builder via encode.
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for row in rows {
            for value in row {
                query = match value {
                    TransformedValue::Null | TransformedValue::Preserve => {
                        query.bind(Option::<String>::None)
                    }
                    TransformedValue::Bool(b) => query.bind(*b),
                    TransformedValue::Int(i) => query.bind(*i),
                    TransformedValue::UInt(u) => query.bind(*u),
                    TransformedValue::Float(f) => query.bind(*f),
                    TransformedValue::Text(s) => query.bind(s.clone()),
                    TransformedValue::Bytes(b) => query.bind(b.clone()),
                };
            }
        }

        query.execute(self.dest).await.map_err(|e| {
            DumplingError::Database(format!("insert into `{}` failed: {e}", info.name))
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum KeyPart {
    I64(i64),
    U64(u64),
    Text(String),
    Bytes(Vec<u8>),
}

fn extract_key(row: &MySqlRow, info: &TableInfo, key_cols: &[String]) -> Result<Vec<KeyPart>> {
    let mut parts = Vec::new();
    for key in key_cols {
        let idx = info
            .columns
            .iter()
            .position(|c| c.name == *key)
            .ok_or_else(|| {
                DumplingError::Validation(format!("key column `{key}` missing from metadata"))
            })?;
        parts.push(row_to_key_part(row, idx)?);
    }
    Ok(parts)
}

fn row_to_key_part(row: &MySqlRow, idx: usize) -> Result<KeyPart> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DumplingError::Database(format!("read key column failed: {e}")))?;
    if raw.is_null() {
        return Err(DumplingError::Database(
            "NULL in keyset pagination key is unsupported".into(),
        ));
    }
    let type_name = row.columns()[idx].type_info().name().to_ascii_lowercase();
    if matches!(
        type_name.as_str(),
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "year"
    ) {
        if let Ok(v) = row.try_get::<i64, _>(idx) {
            return Ok(KeyPart::I64(v));
        }
        if let Ok(v) = row.try_get::<u64, _>(idx) {
            return Ok(KeyPart::U64(v));
        }
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return Ok(KeyPart::Bytes(v));
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Ok(KeyPart::Text(v));
    }
    Err(DumplingError::Database(format!(
        "unsupported key column type `{type_name}`"
    )))
}

fn row_to_transformed(row: &MySqlRow, idx: usize) -> Result<TransformedValue> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DumplingError::Database(format!("read column failed: {e}")))?;
    if raw.is_null() {
        return Ok(TransformedValue::Null);
    }
    let type_name = row.columns()[idx].type_info().name().to_ascii_lowercase();
    match type_name.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "year" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                Ok(TransformedValue::Int(v))
            } else if let Ok(v) = row.try_get::<u64, _>(idx) {
                Ok(TransformedValue::UInt(v))
            } else {
                Ok(TransformedValue::Text(
                    row_value_as_text(row, idx)?.unwrap_or_default(),
                ))
            }
        }
        "float" | "double" | "decimal" | "numeric" | "newdecimal" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                Ok(TransformedValue::Float(v))
            } else if let Ok(v) = row.try_get::<String, _>(idx) {
                Ok(TransformedValue::Text(v))
            } else {
                Ok(TransformedValue::Text(
                    row_value_as_text(row, idx)?.unwrap_or_default(),
                ))
            }
        }
        "bit" | "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
            let v: Vec<u8> = row
                .try_get(idx)
                .map_err(|e| DumplingError::Database(format!("read binary column failed: {e}")))?;
            Ok(TransformedValue::Bytes(v))
        }
        _ => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                Ok(TransformedValue::Text(v))
            } else if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                Ok(TransformedValue::Bytes(v))
            } else {
                Err(DumplingError::Database(format!(
                    "unsupported column type `{type_name}`"
                )))
            }
        }
    }
}

fn row_value_as_text(row: &MySqlRow, idx: usize) -> Result<Option<String>> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| DumplingError::Database(format!("read column failed: {e}")))?;
    if raw.is_null() {
        return Ok(None);
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return Ok(Some(v));
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return Ok(Some(String::from_utf8_lossy(&v).into_owned()));
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return Ok(Some(v.to_string()));
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return Ok(Some(v.to_string()));
    }
    Ok(Some(String::new()))
}

fn yaml_sql_literal(v: &serde_yaml::Value) -> Result<String> {
    match v {
        serde_yaml::Value::Null => Ok("NULL".into()),
        serde_yaml::Value::Bool(b) => Ok(if *b { "1" } else { "0" }.into()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::String(s) => {
            // Escape single quotes for SQL string literal.
            Ok(format!("'{}'", s.replace('\'', "''")))
        }
        _ => Err(DumplingError::Validation(
            "constant value type not supported in source-side SQL".into(),
        )),
    }
}
