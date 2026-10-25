//! Integration tests against Docker Compose MySQL.
//!
//! Requires:
//!   docker compose up -d --wait
//!   DUMPLING_SOURCE_URL / DUMPLING_DEST_URL set (see docker-compose.yml)
//!
//! When MySQL is unavailable these tests are skipped (not failed).

use dumpling::config::Config;
use dumpling::copy::CopyEngine;
use dumpling::db::{self, introspect};
use dumpling::planner::{build_plan, estimate_rows};
use dumpling::progress::Reporter;
use dumpling::safety::assert_destination_writable;
use dumpling::transform::TransformRegistry;
use sqlx::Row;
use std::path::PathBuf;
use std::time::Duration;

fn mysql_urls() -> Option<(String, String)> {
    let src = std::env::var("DUMPLING_SOURCE_URL").ok()?;
    let dst = std::env::var("DUMPLING_DEST_URL").ok()?;
    Some((src, dst))
}

async fn mysql_reachable(url: &str) -> bool {
    match tokio::time::timeout(Duration::from_secs(3), db::mysql::connect(url, "test")).await {
        Ok(Ok(pool)) => db::mysql::ping(&pool).await.is_ok(),
        _ => false,
    }
}

fn test_config_yaml(src: &str, dst: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dumpling.test.yml");
    let raw = std::fs::read_to_string(path).unwrap();
    // Fixture uses env interpolation; set env for parse.
    unsafe {
        std::env::set_var("DUMPLING_SOURCE_URL", src);
        std::env::set_var("DUMPLING_DEST_URL", dst);
        std::env::set_var("DUMPLING_TRANSFORM_SECRET", "integration-test-secret");
        std::env::set_var("DUMPLING_ALLOW_REPLACE", "1");
    }
    raw
}

#[tokio::test]
async fn dry_run_does_not_mutate_destination() {
    let Some((src, dst)) = mysql_urls() else {
        eprintln!("skip: DUMPLING_SOURCE_URL/DEST_URL not set");
        return;
    };
    if !mysql_reachable(&src).await || !mysql_reachable(&dst).await {
        eprintln!("skip: MySQL not reachable");
        return;
    }

    let yaml = test_config_yaml(&src, &dst);
    let config = Config::parse_yaml(&yaml).unwrap();
    let resolved = config
        .resolve(
            PathBuf::from("tests/fixtures/dumpling.test.yml"),
            None,
            None,
            None,
            true,
            true,
        )
        .unwrap();

    let dest = db::mysql::connect(&resolved.dest_url, "destination")
        .await
        .unwrap();
    // Ensure destination starts empty for users.
    sqlx::query("DELETE FROM order_items")
        .execute(&dest)
        .await
        .ok();
    sqlx::query("DELETE FROM payments")
        .execute(&dest)
        .await
        .ok();
    sqlx::query("DELETE FROM orders").execute(&dest).await.ok();
    sqlx::query("DELETE FROM users").execute(&dest).await.ok();
    sqlx::query("DELETE FROM products")
        .execute(&dest)
        .await
        .ok();

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&dest)
        .await
        .unwrap();

    assert_destination_writable(&resolved).unwrap();
    let source = db::mysql::connect(&resolved.source_url, "source")
        .await
        .unwrap();
    let schema = introspect::introspect(&source).await.unwrap();
    let registry = TransformRegistry::with_builtins();
    let mut plan = build_plan(&resolved, &schema, &registry).unwrap();
    estimate_rows(&source, &mut plan).await.ok();
    assert!(!plan.selected_tables.is_empty());

    // dry_run path: CopyEngine returns immediately
    let reporter = Reporter::new(true);
    let engine = CopyEngine {
        resolved: &resolved,
        source: &source,
        dest: &dest,
        source_schema: &schema,
        plan: &plan,
        registry: &registry,
        reporter: &reporter,
    };
    engine.run().await.unwrap();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&dest)
        .await
        .unwrap();
    assert_eq!(before.0, after.0);
}

#[tokio::test]
async fn sync_copies_anonymizes_and_preserves_fks() {
    let Some((src, dst)) = mysql_urls() else {
        eprintln!("skip: DUMPLING_SOURCE_URL/DEST_URL not set");
        return;
    };
    if !mysql_reachable(&src).await || !mysql_reachable(&dst).await {
        eprintln!("skip: MySQL not reachable");
        return;
    }

    let yaml = test_config_yaml(&src, &dst);
    let config = Config::parse_yaml(&yaml).unwrap();
    let resolved = config
        .resolve(
            PathBuf::from("tests/fixtures/dumpling.test.yml"),
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();

    assert_destination_writable(&resolved).unwrap();
    let source = db::mysql::connect(&resolved.source_url, "source")
        .await
        .unwrap();
    let dest = db::mysql::connect(&resolved.dest_url, "destination")
        .await
        .unwrap();
    let schema = introspect::introspect(&source).await.unwrap();
    let registry = TransformRegistry::with_builtins();
    let plan = build_plan(&resolved, &schema, &registry).unwrap();
    let reporter = Reporter::new(true);

    let engine = CopyEngine {
        resolved: &resolved,
        source: &source,
        dest: &dest,
        source_schema: &schema,
        plan: &plan,
        registry: &registry,
        reporter: &reporter,
    };
    engine.run().await.unwrap();

    let user_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&dest)
        .await
        .unwrap();
    // Fixture has 2 active users (carol is deleted and filtered).
    assert_eq!(user_count.0, 2);

    let emails: Vec<String> = sqlx::query("SELECT email FROM users")
        .fetch_all(&dest)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<String, _>("email"))
        .collect();
    for email in &emails {
        assert!(
            email.ends_with(".invalid"),
            "expected anonymized email, got {email}"
        );
        assert!(!email.contains("alice@example.com"));
        assert!(!email.contains("bob@example.com"));
    }

    let pw: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE password_hash IS NOT NULL")
        .fetch_one(&dest)
        .await
        .unwrap();
    assert_eq!(pw.0, 0, "password_hash should be nulled");

    // Orders filtered to 2025+
    let order_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM orders")
        .fetch_one(&dest)
        .await
        .unwrap();
    assert_eq!(order_count.0, 2);

    // FK integrity: every order.user_id exists
    let orphans: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM orders o LEFT JOIN users u ON u.id = o.user_id WHERE u.id IS NULL",
    )
    .fetch_one(&dest)
    .await
    .unwrap();
    assert_eq!(orphans.0, 0);

    // Exclusions
    let audits: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_logs")
        .fetch_one(&dest)
        .await
        .unwrap();
    assert_eq!(audits.0, 0);

    let card: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM payments WHERE card_number IS NOT NULL")
            .fetch_one(&dest)
            .await
            .unwrap();
    assert_eq!(card.0, 0);
}

#[tokio::test]
async fn production_destination_cannot_be_forced() {
    let Some((src, dst)) = mysql_urls() else {
        eprintln!("skip: DUMPLING_SOURCE_URL/DEST_URL not set");
        return;
    };
    // Build a config that marks destination as production.
    let yaml = format!(
        r#"
version: 1
defaults:
  safe_destination: true
connections:
  production:
    url: "{src}"
    production: true
  also_prod:
    url: "{dst}"
    production: true
    allow_destination: true
source: production
destination: also_prod
mode: merge
tables:
  users: {{}}
"#
    );
    let config = Config::parse_yaml(&yaml).unwrap();
    let resolved = config
        .resolve(PathBuf::from("mem"), None, None, None, true, false)
        .unwrap();
    let err = assert_destination_writable(&resolved).unwrap_err();
    assert!(err.to_string().contains("production"));
}
