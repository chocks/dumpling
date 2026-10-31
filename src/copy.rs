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
use tokio::sync::mpsc;

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
        // Plan every table before touching the destination, so a bad column or
        // transform fails before the first write.
        let plans = self
            .plan
            .load_order
            .iter()
            .map(|t| self.table_plan(t))
            .collect::<Result<Vec<_>>>()?;

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

        // Reader (fetch + transform) and writer (insert) overlap, joined by a
        // small bounded channel: memory stays at a few batches, and the single
        // snapshot and single transaction are kept.
        let (send, recv) = mpsc::channel::<Message>(PIPELINE_DEPTH);
        let (read, write) = tokio::join!(
            self.read_all(src, &plans, ctx, send),
            self.write_all(&mut tx, &plans, recv, !replace),
        );
        // The writer's error explains a reader that saw a closed channel, so
        // report it first. Commit only when BOTH sides finished: a reader
        // failure must never commit a half-copied run.
        let summary = write?;
        read?;

        tx.commit()
            .await
            .map_err(|e| DumplingError::Database(format!("commit failed: {e}")))?;
        Ok(summary)
    }

    async fn write_all(
        &self,
        tx: &mut MySqlConnection,
        plans: &[TablePlan<'_>],
        mut recv: mpsc::Receiver<Message>,
        upsert: bool,
    ) -> Result<Vec<(String, u64)>> {
        let mut summary = Vec::new();
        let mut rows_in_table = 0u64;
        let mut started = Instant::now();
        while let Some(message) = recv.recv().await {
            match message {
                Message::Rows(i, rows) => {
                    let plan = &plans[i];
                    writer::insert_rows(tx, plan.dest, &plan.dest_cols, &rows, upsert).await?;
                    rows_in_table += rows.len() as u64;
                }
                Message::TableDone(i) => {
                    let name = plans[i].name;
                    self.reporter
                        .table_done(name, rows_in_table, started.elapsed());
                    tracing::info!(
                        table = %name,
                        rows = rows_in_table,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "table copied"
                    );
                    summary.push((name.to_string(), rows_in_table));
                    rows_in_table = 0;
                    started = Instant::now();
                }
            }
        }
        Ok(summary)
    }

    async fn read_all(
        &self,
        src: &mut MySqlConnection,
        plans: &[TablePlan<'_>],
        ctx: &mut TransformContext,
        send: mpsc::Sender<Message>,
    ) -> Result<()> {
        for (i, plan) in plans.iter().enumerate() {
            self.read_table(src, i, plan, ctx, &send).await?;
            send_message(&send, Message::TableDone(i)).await?;
        }
        Ok(())
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
            if is_generated(col) {
                tracing::debug!("`{table}.{}` is a generated column; skipped", col.name);
                continue;
            }
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

    /// Everything needed to read one table and write it to the destination.
    fn table_plan(&self, table: &str) -> Result<TablePlan<'_>> {
        let missing = |side: &str| {
            DumplingError::Validation(format!("missing {side} metadata for `{table}`"))
        };
        let (name, info) = self
            .source_schema
            .tables
            .get_key_value(table)
            .ok_or_else(|| missing("source"))?;
        let dest = self
            .dest_schema
            .tables
            .get(table)
            .ok_or_else(|| missing("destination"))?;
        let key = info.page_key().ok_or_else(|| {
            DumplingError::Validation(format!(
                "table `{table}` has no primary key or NOT NULL unique index to page by"
            ))
        })?;
        let columns = self.column_sources(table, info, dest, &key)?;
        let dest_cols: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();

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
            let (Some(s), ColumnSource::Read { kind, .. }) = (slot[idx], &columns[idx].1) else {
                unreachable!("key columns are always read, never transformed");
            };
            key_slots.push((s, *kind));
        }
        Ok(TablePlan {
            name,
            dest,
            dest_cols,
            columns,
            selects,
            slot,
            key,
            key_slots,
        })
    }

    /// Page through one table, transform each batch, and hand it to the writer.
    async fn read_table(
        &self,
        src: &mut MySqlConnection,
        index: usize,
        plan: &TablePlan<'_>,
        ctx: &mut TransformContext,
        send: &mpsc::Sender<Message>,
    ) -> Result<()> {
        let table = plan.name;
        let order = plan
            .key
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let tuple = format!("({order})");
        let marks = format!("({})", vec!["?"; plan.key.len()].join(", "));
        let filter = self
            .plan
            .effective_where
            .get(table)
            .and_then(|f| f.as_deref());
        let with = self
            .plan
            .effective_with
            .get(table)
            .and_then(|w| w.as_deref())
            .unwrap_or("");
        let batch_size = self.resolved.config.defaults.batch_size;

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
                "{with}SELECT {} FROM {}{where_sql} ORDER BY {order} LIMIT {batch_size}",
                plan.selects.join(", "),
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
            let Some(last_row) = rows.last() else {
                return Ok(());
            };

            last = Some(
                plan.key_slots
                    .iter()
                    .map(|(s, kind)| value::read_cell(last_row, *s, *kind))
                    .collect::<Result<_>>()?,
            );

            let mut out_rows = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut out = Vec::with_capacity(plan.columns.len());
                for (i, (_, source)) in plan.columns.iter().enumerate() {
                    out.push(match source {
                        ColumnSource::Fixed(v) => v.clone(),
                        ColumnSource::Read { kind, .. } => {
                            value::read_cell(row, plan.slot[i].unwrap(), *kind)?
                        }
                        ColumnSource::Transform { spec, .. } => {
                            let raw =
                                match value::read_cell(row, plan.slot[i].unwrap(), Kind::Text)? {
                                    TransformedValue::Text(t) => Some(t),
                                    _ => None,
                                };
                            self.registry.apply(spec, raw.as_deref(), ctx)?
                        }
                    });
                }
                out_rows.push(out);
            }

            let fetched = rows.len();
            drop(rows); // raw values are gone once the batch is transformed
            send_message(send, Message::Rows(index, out_rows)).await?;
            if fetched < batch_size {
                return Ok(());
            }
        }
    }
}

/// Batches in flight between reader and writer. Small on purpose: this bounds
/// memory and is enough to overlap network and CPU work.
const PIPELINE_DEPTH: usize = 2;

enum Message {
    Rows(usize, Vec<Vec<TransformedValue>>),
    TableDone(usize),
}

async fn send_message(send: &mpsc::Sender<Message>, message: Message) -> Result<()> {
    send.send(message)
        .await
        .map_err(|_| DumplingError::Database("writer stopped before the copy finished".into()))
}

struct TablePlan<'a> {
    name: &'a str,
    dest: &'a TableInfo,
    dest_cols: Vec<String>,
    columns: Vec<(String, ColumnSource)>,
    selects: Vec<String>,
    slot: Vec<Option<usize>>,
    key: Vec<String>,
    key_slots: Vec<(usize, Kind)>,
}

/// VIRTUAL/STORED GENERATED columns cannot be written (MySQL error 3105).
fn is_generated(col: &crate::db::introspect::ColumnInfo) -> bool {
    let extra = col.extra.to_ascii_lowercase();
    extra.contains("virtual generated") || extra.contains("stored generated")
}
