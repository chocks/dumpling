# Dumpling 🥟

[![Crates.io Version](https://img.shields.io/crates/v/dumpling-cli.svg)](https://crates.io/crates/dumpling-cli)
[![Crates.io](https://img.shields.io/badge/crates.io-dumpling--cli-orange)](https://crates.io/crates/dumpling-cli)

**Copy production. Lose the secrets. Keep the bugs.**

Dumpling copies a MySQL database into a local one and anonymizes sensitive
columns on the way. It follows the workflow of
[pgsync](https://github.com/ankane/pgsync): a config file, a source, a
destination, and a safety check before it writes anything.

- Copies selected tables, with row filters that keep foreign keys valid
- Replaces emails, names, phones, addresses, dates; nulls out secrets
- Reads in bounded batches from one consistent snapshot; writes in one transaction
- Refuses to write to production-looking or unapproved destinations

It does **not** create schema (the destination tables must already exist) and
it is not a guarantee of anonymity: review your columns with `dumpling scan`.

## Install

```bash
cargo install dumpling-cli     # installs the `dumpling` binary (Rust 1.94+)
```

## Quick start

The demo below needs Rust and Docker. It builds from source.

```bash
cargo build --release
docker compose up -d --wait        # a source and a destination MySQL with sample data

export DUMPLING_SOURCE_URL='mysql://dumpling:dumpling@127.0.0.1:33061/dumpling_src'
export DUMPLING_DEST_URL='mysql://dumpling:dumpling@127.0.0.1:33062/dumpling_dst'
export DUMPLING_TRANSFORM_SECRET="$(openssl rand -hex 32)"
export DUMPLING_ALLOW_REPLACE=1    # the sample config uses `mode: replace`

alias dumpling=./target/release/dumpling
dumpling -c tests/fixtures/dumpling.test.yml sync --dry-run   # look at the plan
dumpling -c tests/fixtures/dumpling.test.yml sync --force     # copy
```

For your own databases, run `dumpling init` and edit `.dumpling.yml`.

## A minimal config

```yaml
version: 1

connections:
  production:
    url: ${DUMPLING_SOURCE_URL}
  local:
    url: ${DUMPLING_DEST_URL}
    allow_destination: true     # you confirm this is a dev database

source: production
destination: local
mode: merge                     # or `replace` (needs --force)

tables:
  users:
    where: "status != 'deleted'"
    columns:
      email: { transform: unique_email }
      first_name: { transform: fake_first_name }
      password_hash: { transform: "null" }
  orders: {}

exclude: [audit_logs, sessions]
```

Passwords stay in environment variables, never in the file.

## Commands

| Command | What it does |
|---|---|
| `dumpling init` | Write a commented `.dumpling.yml` |
| `dumpling validate` | Check config, connections and the plan |
| `dumpling scan` | List columns that look sensitive |
| `dumpling sync` | Copy and anonymize |
| `dumpling sync --dry-run` | Show the plan and row counts; writes nothing |
| `dumpling sync --tables a,b` / `--group name` / `--env name` | Narrow or switch what is synced |
| `dumpling sync --force` | Allow `mode: replace`. Never overrides production protection |

Exit codes: `0` ok, `3` config, `4` safety, `5` database, `6` validation, `1` internal.

## More

- [Configuration, transforms, safety and limitations](docs/CONFIGURATION.md)
- [How it works](docs/ARCHITECTURE.md)
- [Releasing](docs/RELEASING.md)
- [Status and what is verified](docs/PROGRESS.md)

## Development

```bash
docker compose up -d --wait
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Integration tests create their own databases on the compose servers and fail if
MySQL is not running (`DUMPLING_SKIP_INTEGRATION=1` skips them on purpose).

## License

MIT, see [LICENSE](LICENSE).
