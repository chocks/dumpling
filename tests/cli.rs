//! End-to-end tests of the `dumpling` binary: exit codes and what reaches the
//! destination. Uses the docker-compose MySQL pair.

mod common;
use common::*;
use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dumpling"));
    c.env_remove("DUMPLING_TRANSFORM_SECRET")
        .env_remove("DUMPLING_ALLOW_REPLACE");
    c
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dumpling-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(name: &str, yaml: &str) -> PathBuf {
    let path = tmp_dir(name).join("dumpling.yml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn run(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut c = bin();
    c.args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn all_output(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const SCHEMA: &str =
    "CREATE TABLE users (id INT PRIMARY KEY, email VARCHAR(100) NOT NULL) ENGINE=InnoDB;";
const SEED: &str = "INSERT INTO users VALUES (1, 'alice@real.com'), (2, 'bob@real.com');";
const RULES: &str =
    "mode: merge\ntables:\n  users:\n    columns:\n      email: { transform: unique_email }\n";

#[test]
fn init_writes_a_config_and_refuses_to_overwrite() {
    let dir = tmp_dir("init");
    let path = dir.join(".dumpling.yml");
    let _ = std::fs::remove_file(&path);
    let p = path.to_str().unwrap();
    assert_eq!(code(&run(&["init", "--path", p], &[])), 0);
    assert!(path.exists());
    assert_eq!(
        code(&run(&["init", "--path", p], &[])),
        3,
        "must not overwrite"
    );
    assert_eq!(code(&run(&["init", "--path", p, "--force"], &[])), 0);
}

#[test]
fn invalid_configs_exit_with_actionable_codes() {
    let unknown_key = write_config("badkey", "version: 1\nbogus: true\nconnections: {}\n");
    let out = run(&["-c", unknown_key.to_str().unwrap(), "validate"], &[]);
    assert_eq!(code(&out), 3, "{}", all_output(&out));

    let missing_env = write_config(
        "missingenv",
        "version: 1\nconnections:\n  a: { url: \"${DUMPLING_TEST_UNSET_VAR}\" }\n  b: { url: \"mysql://x@127.0.0.1/y\" }\nsource: a\ndestination: b\n",
    );
    let out = run(&["-c", missing_env.to_str().unwrap(), "validate"], &[]);
    assert_eq!(code(&out), 3);
    assert!(all_output(&out).contains("DUMPLING_TEST_UNSET_VAR"));
}

#[tokio::test]
async fn unknown_transform_is_a_validation_error() {
    let Some(fx) = Fixture::with_schema("cliunk", SCHEMA, SEED).await else {
        return;
    };
    let yaml = fx.yaml(
        "",
        "tables:\n  users:\n    columns:\n      email: { transform: make_pretty }\n",
    );
    let cfg = write_config("unk", &yaml);
    let out = run(&["-c", cfg.to_str().unwrap(), "validate"], &[]);
    assert_eq!(code(&out), 6, "{}", all_output(&out));
    assert!(all_output(&out).contains("make_pretty"));
}

#[tokio::test]
async fn sync_dry_run_then_real_copy() {
    let Some(fx) = Fixture::with_schema("clisync", SCHEMA, SEED).await else {
        return;
    };
    let cfg = write_config("sync", &fx.yaml("  transform_secret: s", RULES));
    let c = cfg.to_str().unwrap();

    let out = run(&["-c", c, "sync", "--dry-run"], &[]);
    assert_eq!(code(&out), 0, "{}", all_output(&out));
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM users").await, 0);

    let out = run(&["-c", c, "sync"], &[]);
    assert_eq!(code(&out), 0, "{}", all_output(&out));
    assert_eq!(ids(&fx.dst, "users").await, vec![1, 2]);

    // Neither connection passwords nor source values appear in the output.
    let text = all_output(&out);
    assert!(!text.contains("root:root"), "{text}");
    assert!(!text.contains("alice@real.com"), "{text}");
    assert_eq!(
        count(
            &fx.dst,
            "SELECT COUNT(*) FROM users WHERE email LIKE '%real.com'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn production_destination_is_refused_even_with_force() {
    let Some(fx) = Fixture::with_schema("cliprod", SCHEMA, SEED).await else {
        return;
    };
    let yaml = fx.yaml("", RULES).replace(
        "    allow_destination: true",
        "    allow_destination: true\n    production: true",
    );
    let cfg = write_config("prod", &yaml);
    let out = run(
        &["-c", cfg.to_str().unwrap(), "sync", "--force"],
        &[("DUMPLING_ALLOW_REPLACE", "1")],
    );
    assert_eq!(code(&out), 4, "{}", all_output(&out));
    assert_eq!(count(&fx.dst, "SELECT COUNT(*) FROM users").await, 0);
}

#[tokio::test]
async fn replace_without_force_is_refused() {
    let Some(fx) = Fixture::with_schema("clireplace", SCHEMA, SEED).await else {
        return;
    };
    let yaml = fx.yaml("", &RULES.replace("merge", "replace"));
    let cfg = write_config("replace", &yaml);
    let out = run(&["-c", cfg.to_str().unwrap(), "sync"], &[]);
    assert_eq!(code(&out), 4, "{}", all_output(&out));
}

/// Kill the process while it is copying. The destination transaction is never
/// committed, so the destination holds nothing or everything, never a fragment.
#[tokio::test]
async fn killing_a_sync_midway_never_leaves_a_partial_destination() {
    const ROWS: i64 = 300_000;
    let schema = "CREATE TABLE big (id INT PRIMARY KEY, v VARCHAR(40)) ENGINE=InnoDB;";
    let seed = format!(
        "SET SESSION cte_max_recursion_depth = {ROWS}; \
         INSERT INTO big WITH RECURSIVE n AS (SELECT 1 i UNION ALL SELECT i+1 FROM n WHERE i < {ROWS}) \
         SELECT i, CONCAT('value-', i) FROM n;"
    );
    let Some(fx) = Fixture::with_schema("clikill", schema, &seed).await else {
        return;
    };
    let cfg = write_config(
        "kill",
        &fx.yaml("  batch_size: 500", "mode: merge\ntables:\n  big: {}\n"),
    );
    let mut child = bin()
        .args(["-c", cfg.to_str().unwrap(), "sync"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    // A full run takes seconds, so the process must still be copying here.
    assert!(
        child.try_wait().unwrap().is_none(),
        "sync finished before the kill"
    );
    let _ = child.kill();
    let _ = child.wait();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let n = count(&fx.dst, "SELECT COUNT(*) FROM big").await;
    assert!(n == 0 || n == ROWS, "partial destination: {n} rows");
}
