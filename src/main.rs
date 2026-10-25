use dumpling::cli::{Cli, Commands};
use dumpling::config::{self, Config, DEFAULT_CONFIG_FILE, find_config};
use dumpling::copy::CopyEngine;
use dumpling::db;
use dumpling::error::{DumplingError, ExitCode, Result};
use dumpling::planner::{build_plan, estimate_rows};
use dumpling::progress::Reporter;
use dumpling::safety::assert_destination_writable;
use dumpling::transform::{TransformRegistry, scan_column_name};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode as StdExitCode;

#[tokio::main]
async fn main() -> StdExitCode {
    let cli = Cli::parse_args();
    init_tracing(cli.quiet, cli.verbose);

    match run(cli).await {
        Ok(()) => StdExitCode::from(ExitCode::Success.as_i32() as u8),
        Err(err) => {
            eprintln!("error: {err}");
            let code = err.exit_code().as_i32();
            StdExitCode::from(code.clamp(0, 255) as u8)
        }
    }
}

fn init_tracing(quiet: bool, verbose: u8) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Version => {
            println!("dumpling {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Commands::Init { force, path } => cmd_init(force, path),
        Commands::Validate => {
            let path = find_config(cli.config.as_deref())?;
            cmd_validate(&path, cli.quiet).await
        }
        Commands::Scan => {
            let path = find_config(cli.config.as_deref())?;
            cmd_scan(&path, cli.quiet).await
        }
        Commands::Sync {
            dry_run,
            tables,
            group,
            env,
            force,
        } => {
            let path = find_config(cli.config.as_deref())?;
            cmd_sync(&path, dry_run, tables, group, env, force, cli.quiet).await
        }
    }
}

fn cmd_init(force: bool, path: Option<PathBuf>) -> Result<()> {
    let path = path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE));
    if path.exists() && !force {
        return Err(DumplingError::Config(format!(
            "`{}` already exists; pass --force to overwrite",
            path.display()
        )));
    }
    fs::write(&path, config::example_config_yaml())?;
    println!("Wrote {}", path.display());
    println!("Next: set DUMPLING_SOURCE_URL / DUMPLING_DEST_URL and run `dumpling validate`.");
    Ok(())
}

async fn load_resolved(
    path: &std::path::Path,
    env: Option<&str>,
    tables: Option<Vec<String>>,
    group: Option<String>,
    force: bool,
    dry_run: bool,
) -> Result<config::ResolvedConfig> {
    let _ = dotenvy::dotenv();
    let config = Config::load(path)?;
    config.resolve(path.to_path_buf(), env, tables, group, force, dry_run)
}

async fn cmd_validate(path: &std::path::Path, quiet: bool) -> Result<()> {
    let reporter = Reporter::new(quiet);
    let resolved = load_resolved(path, None, None, None, false, true).await?;
    let registry = TransformRegistry::with_builtins();

    reporter.info(&format!("Config       {}", path.display()));
    reporter.info(&format!("Source       {}", resolved.source_name));
    reporter.info(&format!("Destination  {}", resolved.dest_name));

    for (table, cfg) in &resolved.config.tables {
        for (col, col_cfg) in &cfg.columns {
            registry
                .validate_spec(&col_cfg.transform)
                .map_err(|e| DumplingError::Validation(format!("{table}.{col}: {e}")))?;
        }
    }

    let source = db::mysql::connect(&resolved.source_url, "source").await?;
    db::mysql::ping(&source).await?;
    let schema = db::introspect::introspect(&source).await?;
    reporter.info(&format!(
        "Source schema `{}`: {} tables",
        schema.database,
        schema.tables.len()
    ));

    let _dest = db::mysql::connect(&resolved.dest_url, "destination").await?;
    assert_destination_writable(&resolved)?;

    let plan = build_plan(&resolved, &schema, &registry)?;
    reporter.info(&format!(
        "Plan OK: {} tables, {} transforms",
        plan.selected_tables.len(),
        plan.transform_count
    ));
    for w in &plan.warnings {
        reporter.info(&format!("warning: {w}"));
    }

    println!("✓ Validation passed");
    Ok(())
}

async fn cmd_scan(path: &std::path::Path, quiet: bool) -> Result<()> {
    let reporter = Reporter::new(quiet);
    let resolved = load_resolved(path, None, None, None, false, true).await?;
    let source = db::mysql::connect(&resolved.source_url, "source").await?;
    let schema = db::introspect::introspect(&source).await?;
    let registry = TransformRegistry::with_builtins();

    reporter.info(&format!(
        "Scanning `{}` for potentially sensitive columns...\n",
        schema.database
    ));

    let mut findings = 0usize;
    for (table, info) in &schema.tables {
        if resolved.config.exclude.iter().any(|e| e == table) {
            continue;
        }
        for col in &info.columns {
            let configured = resolved
                .config
                .tables
                .get(table)
                .and_then(|t| t.columns.get(&col.name));

            if let Some(cfg) = configured {
                println!(
                    "  {table}.{}  configured → {}",
                    col.name,
                    cfg.transform.name()
                );
                findings += 1;
                continue;
            }

            if let Some(h) = scan_column_name(table, &col.name) {
                println!(
                    "  {}  heuristic ({}) — no transform configured",
                    h.column, h.reason
                );
                findings += 1;
            }
        }
    }

    println!();
    println!(
        "Found {findings} notable column(s). Known transforms: {}",
        registry.known_names().join(", ")
    );
    Ok(())
}

async fn cmd_sync(
    path: &std::path::Path,
    dry_run: bool,
    tables: Option<Vec<String>>,
    group: Option<String>,
    env: Option<String>,
    force: bool,
    quiet: bool,
) -> Result<()> {
    let reporter = Reporter::new(quiet);
    let resolved = load_resolved(path, env.as_deref(), tables, group, force, dry_run).await?;
    let registry = TransformRegistry::with_builtins();

    reporter.banner(&resolved);
    assert_destination_writable(&resolved)?;

    reporter.planning_start();
    let source = db::mysql::connect(&resolved.source_url, "source").await?;
    let dest = db::mysql::connect(&resolved.dest_url, "destination").await?;
    let schema = db::introspect::introspect(&source).await?;

    let dest_schema = db::introspect::introspect(&dest).await?;
    let mut plan = build_plan(&resolved, &schema, &registry)?;
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

    if dry_run {
        reporter.success(true);
        return Ok(());
    }

    let engine = CopyEngine {
        resolved: &resolved,
        source: &source,
        dest: &dest,
        source_schema: &schema,
        plan: &plan,
        registry: &registry,
        reporter: &reporter,
    };
    engine.run().await?;
    reporter.success(false);
    Ok(())
}
