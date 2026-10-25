//! Streaming MySQL → MySQL copy engine.
//!
//! Flow: one read-only consistent-snapshot transaction on the source, one
//! transaction on the destination. Rows move in keyset-paginated batches, so
//! memory is bounded by `batch_size`. Any error rolls the destination back.
//!
//! Privacy boundary:
//! - `null` and `constant` columns are never selected from the source.
//! - Client-side transforms (everything else) read the raw value into this
//!   process for the current batch only; it is never written to disk or logged.

use crate::config::{DestinationMode, ResolvedConfig, TableConfig};
use crate::db::MySqlPool;
use crate::db::introspect::{SchemaSnapshot, TableInfo};
use crate::db::value::{self, Kind, quote_ident};
use crate::db::writer;
use crate::error::{DumplingError, Result};
use crate::planner::SyncPlan;
use crate::progress::Reporter;
use crate::transform::{TransformContext, TransformKind, TransformRegistry, TransformedValue};
use sqlx::{AssertSqlSafe, MySqlConnection};
use std::time::Instant;

pub struct CopyEngine<'a> {
    pub resolved: &'a ResolvedConfig,
    pub source: &'a MySqlPool,
    pub dest: &'a MySqlPool,
    pub source_schema: &'a SchemaSnapshot,
    pub dest_schema: &'a SchemaSnapshot,
    pub plan: &'a SyncPlan,
    pub registry: &'a TransformRegistry,
    pub reporter: &'a Reporter,
}

/// How one destination column is produced.
enum ColumnSource {
    /// Not read from the source; value known up front (`null`, `constant`).
    Fixed(TransformedValue),
    /// Read from the source as-is.
    Read { select: String, kind: Kind },
    /// Read as text and passed through a client-side transform.
    Transform {
        select: String,
        spec: crate::config::TransformSpec,
    },
}

impl CopyEngine<'_> {
    pub async fn run(&self) -> Result<Vec<(String, u64)>> {
        if self.resolved.dry_run {
            return Ok(vec![]);
        }
        let mut ctx = match self.resolved.config.defaults.transform_secret.as_deref() {
            Some(secret) => TransformContext::new(secret),
            None => {
                if self.plan.transform_count > 0 {
                    tracing::warn!(
                        "no transform secret set: using a random one for this run, so \
                         replacements will differ between runs. Set DUMPLING_TRANSFORM_SECRET \
                         for stable output"
                    );
                }
                TransformContext::random()
            }
        };

        self.reporter.copying_start();

        // Read-only snapshot: every table is read at the same point in time,
        // and Dumpling cannot write to the source even by mistake.
        let mut src = self
            .source
            .acquire()
            .await
            .map_err(|e| DumplingError::Database(format!("source connection failed: {e}")))?;
        sqlx::raw_sql("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
            .execute(&mut *src)
            .await
            .map_err(|e| DumplingError::Database(format!("source snapshot failed: {e}")))?;

        let result = self.copy_all(&mut src, &mut ctx).await;
        let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *src).await;
        result
    }

    async fn copy_all(
        &self,
        src: &mut MySqlConnection,
        ctx: &mut TransformContext,
    ) -> Result<Vec<(String, u64)>> {
        let mut tx =
            self.dest.begin().await.map_err(|e| {
                DumplingError::Database(format!("destination transaction failed: {e}"))
            })?;

        let replace = matches!(self.resolved.mode, DestinationMode::Replace);
        if replace {
            for table in self.plan.load_order.iter().rev() {
                writer::delete_all(&mut tx, table).await?;
            }
        }

        let mut summary = Vec::new();
        for table in &self.plan.load_order {
            let started = Instant::now();
            let rows = self.copy_table(src, &mut tx, table, ctx, !replace).await?;
            self.reporter.table_done(table, rows, started.elapsed());
            tracing::info!(
                table = %table,
                rows,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "table copied"
            );
            summary.push((table.clone(), rows));
        }

        // Dropping `tx` on any early return above rolls everything back.
        tx.commit()
            .await
            .map_err(|e| DumplingError::Database(format!("commit failed: {e}")))?;
        Ok(summary)
    }

    /// Decide how each shared column is produced. Columns present on only one
    /// side are skipped (like pgsync, only shared columns are copied).
    fn column_sources(
        &self,
        table: &str,
        info: &TableInfo,
        dest: &TableInfo,
        key: &[String],
    ) -> Result<Vec<(String, ColumnSource)>> {
        let table_cfg: Option<&TableConfig> = self.resolved.config.tables.get(table);
        let anonymize = self.resolved.config.defaults.anonymize;
        let mut out = Vec::new();
        for col in &info.columns {
            if dest.column(&col.name).is_none() {
                tracing::warn!(
                    "`{table}.{}` does not exist in the destination; skipped",
                    col.name
                );
                continue;
            }
            let spec = table_cfg
                .and_then(|c| c.columns.get(&col.name))
                .map(|c| &c.transform)
                .filter(|_| anonymize);
            let kind = spec.map(|s| self.registry.kind(s)).transpose()?;
            let read = || {
                value::read_plan(col)
                    .map_err(|e| DumplingError::Validation(format!("`{table}`: {e}")))
            };
            let source = match (spec, kind) {
                (None, _) | (_, Some(TransformKind::Preserve)) => {
                    let (select, kind) = read()?;
                    ColumnSource::Read { select, kind }
                }
                (_, Some(TransformKind::Null)) => ColumnSource::Fixed(TransformedValue::Null),
                (Some(spec), Some(TransformKind::Constant)) => ColumnSource::Fixed(
                    crate::transform::yaml_to_value(spec.constant_value().ok_or_else(|| {
                        DumplingError::Validation("transform `constant` requires `value`".into())
                    })?)?,
                ),
                (Some(spec), _) => ColumnSource::Transform {
                    select: value::text_expr(col)
                        .map_err(|e| DumplingError::Validation(format!("`{table}`: {e}")))?,
                    spec: spec.clone(),
                },
            };
            if key.contains(&col.name) && !matches!(source, ColumnSource::Read { .. }) {
                return Err(DumplingError::Validation(format!(
                    "`{table}.{}` is part of the key used for paging and cannot be transformed",
                    col.name
                )));
            }
            out.push((col.name.clone(), source));
        }
        if out.is_empty() {
            return Err(DumplingError::Validation(format!(
                "table `{table}` shares no columns with the destination"
            )));
        }
        Ok(out)
    }

    async fn copy_table(
        &self,
        src: &mut MySqlConnection,
        tx: &mut MySqlConnection,
        table: &str,
        ctx: &mut TransformContext,
        upsert: bool,
    ) -> Result<u64> {
        let missing = |side: &str| {
            DumplingError::Validation(format!("missing {side} metadata for `{table}`"))
        };
        let info = self
            .source_schema
            .tables
            .get(table)
            .ok_or_else(|| missing("source"))?;
        let dest_info = self
            .dest_schema
            .tables
            .get(table)
            .ok_or_else(|| missing("destination"))?;
        let key = info.page_key().ok_or_else(|| {
            DumplingError::Validation(format!(
                "table `{table}` has no primary key or NOT NULL unique index to page by"
            ))
        })?;
        let columns = self.column_sources(table, info, dest_info, &key)?;
        let dest_cols: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();

        // Selected columns, in order; remember where each destination column
        // lives in the result row.
        let mut selects: Vec<String> = Vec::new();
        let mut slot: Vec<Option<usize>> = Vec::new();
        for (_, source) in &columns {
            match source {
                ColumnSource::Fixed(_) => slot.push(None),
                ColumnSource::Read { select, .. } | ColumnSource::Transform { select, .. } => {
                    slot.push(Some(selects.len()));
                    selects.push(select.clone());
                }
            }
        }
        // Paging key values come from the same result row.
        let mut key_slots = Vec::new();
        for k in &key {
            let idx = columns.iter().position(|(n, _)| n == k).ok_or_else(|| {
                DumplingError::Validation(format!("key column `{table}.{k}` is not shared"))
            })?;
            let Some(s) = slot[idx] else {
                unreachable!("key columns are always read")
            };
            let ColumnSource::Read { kind, .. } = &columns[idx].1 else {
                unreachable!()
            };
            key_slots.push((s, *kind));
        }

        let order = key
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let tuple = format!("({order})");
        let marks = format!("({})", vec!["?"; key.len()].join(", "));
        let filter = self
            .plan
            .effective_where
            .get(table)
            .and_then(|f| f.as_deref());
        let batch_size = self.resolved.config.defaults.batch_size;

        let mut total = 0u64;
        let mut last: Option<Vec<TransformedValue>> = None;
        loop {
            let mut wheres = Vec::new();
            if let Some(f) = filter {
                wheres.push(format!("({f})"));
            }
            if last.is_some() {
                wheres.push(format!("{tuple} > {marks}"));
            }
            let where_sql = if wheres.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", wheres.join(" AND "))
            };
            let sql = format!(
                "SELECT {} FROM {}{where_sql} ORDER BY {order} LIMIT {batch_size}",
                selects.join(", "),
                quote_ident(table)
            );
            // Identifiers are validated/quoted; the filter is trusted admin config.
            let mut q = sqlx::query(AssertSqlSafe(sql));
            if let Some(keys) = &last {
                for k in keys {
                    q = match k {
                        TransformedValue::Int(v) => q.bind(*v),
                        TransformedValue::UInt(v) => q.bind(*v),
                        TransformedValue::Float(v) => q.bind(*v),
                        TransformedValue::Text(v) => q.bind(v.clone()),
                        TransformedValue::Bytes(v) => q.bind(v.clone()),
                        TransformedValue::Bool(v) => q.bind(*v),
                        TransformedValue::Null => unreachable!("page keys are NOT NULL"),
                    };
                }
            }
            let rows = q.fetch_all(&mut *src).await.map_err(|e| {
                DumplingError::Database(format!(
                    "reading `{table}` failed: {}",
                    writer::redact_db_error(&e)
                ))
            })?;
            let Some(last_row) = rows.last() else { break };

            last = Some(
                key_slots
                    .iter()
                    .map(|(s, kind)| value::read_cell(last_row, *s, *kind))
                    .collect::<Result<_>>()?,
            );

            let mut out_rows = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut out = Vec::with_capacity(columns.len());
                for (i, (_, source)) in columns.iter().enumerate() {
                    out.push(match source {
                        ColumnSource::Fixed(v) => v.clone(),
                        ColumnSource::Read { kind, .. } => {
                            value::read_cell(row, slot[i].unwrap(), *kind)?
                        }
                        ColumnSource::Transform { spec, .. } => {
                            let raw = match value::read_cell(row, slot[i].unwrap(), Kind::Text)? {
                                TransformedValue::Text(t) => Some(t),
                                _ => None,
                            };
                            self.registry.apply(spec, raw.as_deref(), ctx)?
                        }
                    });
                }
                out_rows.push(out);
            }

            writer::insert_rows(tx, dest_info, &dest_cols, &out_rows, upsert).await?;
            total += rows.len() as u64;
            if rows.len() < batch_size {
                break;
            }
        }
        Ok(total)
    }
}
