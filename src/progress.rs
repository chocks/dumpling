use crate::config::{DestinationMode, ResolvedConfig};
use crate::planner::SyncPlan;
use console::{Emoji, style};
use std::io::{self, Write};

static DUMPLING: Emoji<'_, '_> = Emoji("🥟 ", "");

pub struct Reporter {
    quiet: bool,
}

impl Reporter {
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    pub fn banner(&self, resolved: &ResolvedConfig) {
        if self.quiet {
            return;
        }
        println!();
        println!("{}{}", DUMPLING, style("Dumpling").bold());
        println!();
        println!("  {:12} {}", style("Source").dim(), resolved.source_name);
        println!("  {:12} {}", style("Destination").dim(), resolved.dest_name);
        println!(
            "  {:12} {}",
            style("Mode").dim(),
            match resolved.mode {
                DestinationMode::Merge => "merge",
                DestinationMode::Replace => "replace",
            }
        );
        println!(
            "  {:12} {}",
            style("Anonymize").dim(),
            if resolved.config.defaults.anonymize {
                "enabled"
            } else {
                "disabled"
            }
        );
        if resolved.dry_run {
            println!(
                "  {:12} {}",
                style("Dry-run").dim(),
                style("yes — no destination writes").yellow()
            );
        }
        println!();
    }

    pub fn planning_start(&self) {
        if self.quiet {
            return;
        }
        println!("{}", style("Planning database copy...").bold());
    }

    pub fn plan_summary(&self, plan: &SyncPlan) {
        if self.quiet {
            return;
        }
        println!(
            "  {} {} tables selected",
            style("✓").green(),
            plan.selected_tables.len()
        );
        println!(
            "  {} {} tables excluded",
            style("✓").green(),
            plan.excluded_tables.len()
        );
        println!(
            "  {} {} transformation rules validated",
            style("✓").green(),
            plan.transform_count
        );
        println!(
            "  {} Foreign-key dependencies inspected",
            style("✓").green()
        );
        if !plan.cycles.is_empty() {
            println!(
                "  {} {} cycle(s) reported (see warnings)",
                style("!").yellow(),
                plan.cycles.len()
            );
        }
        for w in &plan.warnings {
            println!("  {} {}", style("warning:").yellow(), w);
        }
        println!();
        println!("{}", style("Load order:").bold());
        for (i, t) in plan.load_order.iter().enumerate() {
            let estimate = plan
                .estimated_rows
                .get(t)
                .and_then(|v| *v)
                .map(|n| format!("  (~{} rows)", format_count(n)))
                .unwrap_or_default();
            let filter = plan
                .effective_where
                .get(t)
                .and_then(|f| f.as_ref())
                .map(|_| "  [filtered]")
                .unwrap_or("");
            println!("  {:2}. {}{}{}", i + 1, t, filter, estimate);
        }
        println!();
    }

    pub fn copying_start(&self) {
        if self.quiet {
            return;
        }
        println!("{}", style("Copying data...").bold());
    }

    pub fn table_done(&self, table: &str, rows: u64) {
        if self.quiet {
            return;
        }
        println!("  {:20} {:>10} rows", table, format_count(rows));
        let _ = io::stdout().flush();
    }

    pub fn success(&self, dry_run: bool) {
        if self.quiet {
            return;
        }
        println!();
        if dry_run {
            println!(
                "{} {}",
                style("✓").green(),
                style("Dry-run completed — destination was not modified").green()
            );
        } else {
            println!(
                "{} {}",
                style("✓").green(),
                style("Sync completed successfully").green()
            );
        }
        println!();
    }

    pub fn info(&self, msg: &str) {
        if self.quiet {
            return;
        }
        println!("{msg}");
    }
}

fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}
