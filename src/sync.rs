//! Orchestration of a full sync run: connect, introspect, plan, safety-check, copy.
//!
//! Shared by the CLI and by integration tests so both exercise the same path.

use crate::config::ResolvedConfig;
use crate::copy::CopyEngine;
use crate::db;
use crate::error::{DumplingError, Result};
use crate::planner::{build_plan, estimate_rows};
use crate::progress::Reporter;
use crate::safety::assert_destination_writable;
use crate::transform::TransformRegistry;

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub dry_run: bool,
    /// `(table, rows copied)` in load order. Empty for dry runs.
    pub tables: Vec<(String, u64)>,
    /// Exact per-table row counts from the plan (after filters).
    pub estimated_rows: indexmap::IndexMap<String, Option<u64>>,
}

pub async fn run_sync(resolved: &ResolvedConfig, reporter: &Reporter) -> Result<SyncSummary> {
    let registry = TransformRegistry::with_builtins();

    reporter.banner(resolved);
    assert_destination_writable(resolved)?;

    reporter.planning_start();
    let source = db::mysql::connect(&resolved.source_url, "source").await?;
    let dest = db::mysql::connect(&resolved.dest_url, "destination").await?;
    assert_different_databases(&source, &dest).await?;
    let schema = db::introspect::introspect(&source).await?;
    let dest_schema = db::introspect::introspect(&dest).await?;
    let mut plan = build_plan(resolved, &schema, &registry)?;

    for t in &plan.selected_tables {
        if !dest_schema.tables.contains_key(t) {
            return Err(DumplingError::Validation(format!(
                "destination schema `{}` is missing table `{t}` — \
                 Dumpling copies data only; create matching schema first",
                dest_schema.database
            )));
        }
    }

    if let Err(e) = estimate_rows(&source, &mut plan).await {
        tracing::warn!("row estimation skipped: {e}");
    }
    reporter.plan_summary(&plan);

    if resolved.dry_run {
        reporter.success(true);
        return Ok(SyncSummary {
            dry_run: true,
            tables: vec![],
            estimated_rows: plan.estimated_rows,
        });
    }

    let engine = CopyEngine {
        resolved,
        source: &source,
        dest: &dest,
        source_schema: &schema,
        dest_schema: &dest_schema,
        plan: &plan,
        registry: &registry,
        reporter,
    };
    let tables = engine.run().await?;
    reporter.success(false);
    Ok(SyncSummary {
        dry_run: false,
        tables,
        estimated_rows: plan.estimated_rows,
    })
}

/// Two connection entries can name the same database (aliases, tunnels,
/// proxies), and `mode: replace` would then wipe the source. Compare what the
/// servers report about themselves rather than trusting the URLs.
async fn assert_different_databases(source: &db::MySqlPool, dest: &db::MySqlPool) -> Result<()> {
    async fn identity(pool: &db::MySqlPool) -> Result<(String, Option<String>)> {
        sqlx::query_as("SELECT @@server_uuid, DATABASE()")
            .fetch_one(pool)
            .await
            .map_err(|e| DumplingError::Database(format!("identity check failed: {e}")))
    }
    if identity(source).await? == identity(dest).await? {
        return Err(DumplingError::Safety(
            "source and destination are the same database (same server and schema)".into(),
        ));
    }
    Ok(())
}
