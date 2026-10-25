# Dumpling 🥟

**Copy production. Lose the secrets. Keep the bugs.**

Dumpling is a Rust CLI that copies MySQL databases into local development environments while anonymizing sensitive data. It is inspired by the configuration-driven workflow of [`ankane/pgsync`](https://github.com/ankane/pgsync), targeting MySQL first.

## What it does

- Copies selected tables and filtered rows from a source MySQL database to a destination
- Anonymizes configured columns during transfer (with source-side nulling/constants when possible)
- Preserves primary keys and foreign-key load order
- Streams rows in bounded batches (keyset pagination)
- Refuses writes to production-marked or unsafe destinations (`--force` cannot override this)

## What it does **not** do

- Migrate or invent schema/DDL — destination tables must already exist
- Claim that hashing alone is anonymization
- Guarantee cross-engine transactional atomicity on MySQL
- Automatically discover every sensitive column (use `scan` + review)
- Support engines other than MySQL/MariaDB yet

## Install (development)

```bash
# Rust stable (1.85+ recommended; tested on 1.99)
cargo build --release
./target/release/dumpling --help
```

## Quick start

```bash
# 1. Start disposable MySQL pair
docker compose up -d --wait

# 2. Export connection URLs (never commit passwords)
export DUMPLING_SOURCE_URL='mysql://dumpling:dumpling@127.0.0.1:33061/dumpling_src'
export DUMPLING_DEST_URL='mysql://dumpling:dumpling@127.0.0.1:33062/dumpling_dst'
export DUMPLING_TRANSFORM_SECRET='replace-me-with-a-long-random-secret'
export DUMPLING_ALLOW_REPLACE=1   # only for replace mode in CI/noninteractive

# 3. Create config
cargo run -- init --force
# edit .dumpling.yml, or use tests/fixtures/dumpling.test.yml

# 4. Validate & dry-run
cargo run -- --config tests/fixtures/dumpling.test.yml validate
cargo run -- --config tests/fixtures/dumpling.test.yml sync --dry-run

# 5. Sync (replace mode requires --force + DUMPLING_ALLOW_REPLACE in CI)
cargo run -- --config tests/fixtures/dumpling.test.yml sync --force
```

## CLI

| Command | Purpose |
|---------|---------|
| `dumpling init` | Write `.dumpling.yml` from the commented example |
| `dumpling validate` | Parse config, connect, validate transforms & plan |
| `dumpling scan` | Report configured + heuristically sensitive columns |
| `dumpling sync` | Copy + anonymize |
| `dumpling sync --dry-run` | Plan only — **no destination writes** |
| `dumpling sync --tables a,b` | Restrict tables (FK deps still expanded) |
| `dumpling sync --group customer` | Copy a named group |
| `dumpling sync --env staging` | Select environment profile |
| `dumpling sync --force` | Allow configured destructive `replace` (never bypasses production protection) |
| `dumpling version` | Print version |

Exit codes: `0` success, `3` config, `4` safety, `5` database, `6` validation, `1` internal.

## Configuration reference

Default file: `.dumpling.yml` (see `.dumpling.example.yml`).

**Precedence:** CLI flags (`--tables`, `--group`, `--env`, `--force`, `--dry-run`) override environment profile selection; table rules override defaults; missing env vars in `${VAR}` are hard errors.

| Key | Meaning |
|-----|---------|
| `version` | Must be `1` |
| `defaults.batch_size` | Keyset page size (default 2000) |
| `defaults.jobs` | Reserved for future parallel table copy (default 4) |
| `defaults.anonymize` | Apply column transforms |
| `defaults.safe_destination` | Require `allow_destination: true` |
| `defaults.deterministic` | Stable replacements via HMAC identity map |
| `connections.*.url` | `mysql://` URL, usually `${ENV}` |
| `connections.*.production` | Writes always refused |
| `connections.*.allow_destination` | Explicit write opt-in |
| `mode` | `merge` (default) or `replace` |
| `tables.*.where` | Trusted admin SQL boolean expression |
| `tables.*.columns.*.transform` | Transform name |
| `exclude` | Tables never copied |
| `groups` | Named table sets |
| `environments` | Named source/destination pairs for `--env` |

### Transforms

| Name | Where it runs | Notes |
|------|---------------|-------|
| `null` | **Source SQL** | Rejected on `NOT NULL` columns |
| `constant` | **Source SQL** | Requires `value:` |
| `preserve` | Source | Explicit keep — use sparingly |
| `unique_email` | Client | RFC 2606 `.invalid` domain; stable with secret |
| `fake_email` | Client | Reserved-domain emails |
| `fake_first_name` / `fake_last_name` | Client | Deterministic pick from small lists |
| `fake_phone` / `unique_phone` | Client | Synthetic `+1-555-…` |
| `fake_address` | Client | Synthetic street lines |
| `random_date` | Client | Deterministic date in a fixed window |
| `hash` | Client | HMAC-SHA256 hex — **pseudonymization, not anonymization** |

Set `DUMPLING_TRANSFORM_SECRET` for keyed deterministic transforms. Secrets are never logged.

## Production safety

1. Destinations with `production: true` are **never** writable (`--force` ignored).
2. Hostnames matching RDS/Azure/Cloud SQL/`prod` markers are classified production-like and refused.
3. With `safe_destination: true` (default), destinations need `allow_destination: true`.
4. `mode: replace` requires `--force`; noninteractive sessions also need `DUMPLING_ALLOW_REPLACE=1`.
5. Dry-run never executes writes.
6. Connection passwords and row values are redacted from logs/errors.

Hostname heuristics are a backstop, not a substitute for explicit connection flags.

## Permissions

- **Source:** `SELECT` on target tables + `information_schema` (prefer a read-only account).
- **Destination:** `SELECT`, `INSERT`, `UPDATE`, `DELETE` on target tables (no need for `DROP`).

Prefer TLS for remote URLs. Dumpling uses SQLx rustls; certificate verification is **not** disabled by default.

## Performance

- Tune `defaults.batch_size` (1000–5000 is a common range).
- Prefer primary-key keyset pagination (tables without PK/unique keys are rejected).
- Filters use administrator SQL in `WHERE`; keep them sargable when possible.

## MySQL limitations

- Schema must exist on the destination beforehand.
- FK cycles are reported; load order may need deferred constraints (not auto-disabled).
- `TRUNCATE` is avoided in favor of `DELETE` for replace mode to play nicer with FKs.
- Mixed engines / DDL break transaction assumptions — Dumpling does not promise a single atomic snapshot restore.
- Tables without a primary or unique key are unsupported.

## Testing

```bash
docker compose up -d --wait
export DUMPLING_SOURCE_URL='mysql://dumpling:dumpling@127.0.0.1:33061/dumpling_src'
export DUMPLING_DEST_URL='mysql://dumpling:dumpling@127.0.0.1:33062/dumpling_dst'
export DUMPLING_TRANSFORM_SECRET='test-secret'
export DUMPLING_ALLOW_REPLACE=1

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Integration tests in `tests/mysql_integration.rs` skip cleanly if MySQL URLs are unset or unreachable.

## Roadmap

- Parallel per-table copy bounded by `jobs`
- Structured filters (replace free-form SQL)
- Source-side hashing for additional transforms
- Optional schema sync / create-table mode
- PostgreSQL engine backend

## License

MIT — see [LICENSE](LICENSE).
