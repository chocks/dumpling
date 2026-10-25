//! Shared harness for integration tests against the docker-compose MySQL pair.
//!
//! Each test gets its own pair of databases (`dt_<name>_src` on the source
//! server, `dt_<name>_dst` on the destination server), so tests are isolated
//! and can run in parallel. Connections use the compose root account; this is
//! test-only and the data is synthetic.
//!
//! If MySQL is unreachable the test FAILS, unless `DUMPLING_SKIP_INTEGRATION=1`
//! is set. (Silently skipping once hid a broken copy engine.)
#![allow(dead_code)]

use dumpling::config::Config;
use dumpling::error::Result;
use dumpling::progress::Reporter;
use dumpling::sync::{SyncSummary, run_sync};
use sqlx::{MySqlPool, Row};
use std::path::PathBuf;
use std::sync::Once;

const SRC_SERVER: &str = "mysql://root:root@127.0.0.1:33061";
const DST_SERVER: &str = "mysql://root:root@127.0.0.1:33062";

pub struct Fixture {
    pub src: MySqlPool,
    pub dst: MySqlPool,
    pub src_url: String,
    pub dst_url: String,
}

static ENV: Once = Once::new();

fn init_env() {
    ENV.call_once(|| {
        // Noninteractive second confirmation for `mode: replace`.
        unsafe {
            std::env::set_var("DUMPLING_ALLOW_REPLACE", "1");
            // Tests choose their own secret (or none) in the YAML.
            std::env::remove_var("DUMPLING_TRANSFORM_SECRET");
        }
    });
}

async fn server_pool(url: &str) -> Option<MySqlPool> {
    let fut = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(url);
    fut.await.ok()
}

impl Fixture {
    /// Create fresh databases and run the given DDL/DML scripts on each side.
    /// Returns `None` only when `DUMPLING_SKIP_INTEGRATION=1` and MySQL is down.
    pub async fn new(name: &str, src_sql: &str, dst_sql: &str) -> Option<Fixture> {
        init_env();
        let (Some(src_admin), Some(dst_admin)) =
            (server_pool(SRC_SERVER).await, server_pool(DST_SERVER).await)
        else {
            if std::env::var("DUMPLING_SKIP_INTEGRATION").as_deref() == Ok("1") {
                eprintln!("skip: MySQL not reachable (DUMPLING_SKIP_INTEGRATION=1)");
                return None;
            }
            panic!(
                "MySQL is not reachable on 127.0.0.1:33061/33062. Run \
                 `docker compose up -d --wait` (or set DUMPLING_SKIP_INTEGRATION=1)."
            );
        };

        let src_db = format!("dt_{name}_src");
        let dst_db = format!("dt_{name}_dst");
        for (admin, db) in [(&src_admin, &src_db), (&dst_admin, &dst_db)] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS `{db}`; CREATE DATABASE `{db}` \
                 CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"
            )))
            .execute(admin)
            .await
            .unwrap();
        }

        let src_url = format!("{SRC_SERVER}/{src_db}");
        let dst_url = format!("{DST_SERVER}/{dst_db}");
        let src = server_pool(&src_url).await.unwrap();
        let dst = server_pool(&dst_url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(src_sql.to_string()))
            .execute(&src)
            .await
            .unwrap_or_else(|e| panic!("source fixture SQL failed: {e}"));
        sqlx::raw_sql(sqlx::AssertSqlSafe(dst_sql.to_string()))
            .execute(&dst)
            .await
            .unwrap_or_else(|e| panic!("destination fixture SQL failed: {e}"));
        Some(Fixture {
            src,
            dst,
            src_url,
            dst_url,
        })
    }

    /// Same schema on both sides; `seed` runs on the source only.
    pub async fn with_schema(name: &str, schema: &str, seed: &str) -> Option<Fixture> {
        let src_sql = format!("{schema}\n{seed}");
        Self::new(name, &src_sql, schema).await
    }

    /// Full YAML config. `body` is appended after the standard header
    /// (`mode`, `tables`, `exclude`, ...).
    pub fn yaml(&self, defaults: &str, body: &str) -> String {
        format!(
            r#"
version: 1
defaults:
  safe_destination: true
{defaults}
connections:
  src:
    url: "{}"
  dst:
    url: "{}"
    allow_destination: true
source: src
destination: dst
{body}
"#,
            self.src_url, self.dst_url
        )
    }

    pub fn resolve(
        &self,
        yaml: &str,
        force: bool,
        dry_run: bool,
    ) -> dumpling::config::ResolvedConfig {
        Config::parse_yaml(yaml)
            .unwrap()
            .resolve(PathBuf::from("test.yml"), None, None, None, force, dry_run)
            .unwrap()
    }

    pub async fn sync(&self, yaml: &str, force: bool, dry_run: bool) -> Result<SyncSummary> {
        let resolved = self.resolve(yaml, force, dry_run);
        run_sync(&resolved, &Reporter::new(true)).await
    }
}

/// Dump every row of `table` as strings (`CAST AS CHAR`, `HEX` for binary
/// types), ordered by `order_by`, so source and destination can be compared
/// without caring about driver-level type mapping.
pub async fn dump(pool: &MySqlPool, table: &str, order_by: &str) -> Vec<Vec<Option<String>>> {
    let cols = sqlx::query(
        "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap();
    let exprs: Vec<String> = cols
        .iter()
        .map(|r| {
            let name: String = r.get("COLUMN_NAME");
            let ty: String = r.get::<String, _>("DATA_TYPE").to_lowercase();
            match ty.as_str() {
                "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
                    format!("HEX(`{name}`)")
                }
                "bit" => format!("CAST(`{name}`+0 AS CHAR)"),
                _ => format!("CAST(`{name}` AS CHAR)"),
            }
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM `{table}` ORDER BY {order_by}",
        exprs.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (0..exprs.len())
                .map(|i| row.try_get::<Option<String>, _>(i).unwrap())
                .collect()
        })
        .collect()
}

pub async fn count(pool: &MySqlPool, sql: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

pub async fn ids(pool: &MySqlPool, table: &str) -> Vec<i64> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(id AS SIGNED) AS id FROM `{table}` ORDER BY id"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<i64, _>("id"))
    .collect()
}
