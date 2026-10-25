use dumpling::config::Config;
use std::env;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn example_config_requires_env_interpolation() {
    let _g = env_lock();
    unsafe {
        env::remove_var("DUMPLING_SOURCE_URL");
        env::remove_var("DUMPLING_DEST_URL");
    }
    let yaml = include_str!("../.dumpling.example.yml");
    let err = Config::parse_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("DUMPLING_SOURCE_URL") || msg.contains("DUMPLING_DEST_URL"),
        "{msg}"
    );
}

#[test]
fn example_config_parses_with_env() {
    let _g = env_lock();
    unsafe {
        env::set_var(
            "DUMPLING_SOURCE_URL",
            "mysql://ro:x@db.example.com:3306/app",
        );
        env::set_var("DUMPLING_DEST_URL", "mysql://dev:x@127.0.0.1:3307/app_dev");
    }
    let yaml = include_str!("../.dumpling.example.yml");
    let cfg = Config::parse_yaml(yaml).expect("example config should parse");
    assert_eq!(cfg.version, 1);
    assert!(cfg.connections["local"].allow_destination);
    assert!(cfg.connections["production"].production);
    unsafe {
        env::remove_var("DUMPLING_SOURCE_URL");
        env::remove_var("DUMPLING_DEST_URL");
    }
}

#[test]
fn rejects_bad_scheme() {
    let yaml = r#"
version: 1
connections:
  local:
    url: postgres://localhost/db
    allow_destination: true
source: local
destination: local
"#;
    let err = Config::parse_yaml(yaml).unwrap_err();
    assert!(
        err.to_string().contains("unsupported URL scheme")
            || err.to_string().contains("must be different")
            || err.to_string().contains("malformed")
            || err.to_string().contains("postgres")
            || err.to_string().contains("mysql")
    );
}
