# Progress

Status of each phase and how it was verified. "Verified" means exercised by an
automated test against the docker-compose MySQL pair, not mocks.

Run the stack: `docker compose up -d --wait`, then `cargo test`.
Integration tests **fail** if MySQL is unreachable (set
`DUMPLING_SKIP_INTEGRATION=1` to skip deliberately).

## Phase 1 — Foundation (commit `16645c2`)

CLI, config parsing + env interpolation, schema introspection, planner,
destination safety policy, transform registry, README, compose fixtures.

Review at the start of Phase 2 found that the Phase 1 integration tests had been
**silently skipping** (no MySQL running), and the copy engine failed on the first
real run. Phase 1 should be treated as "compiles, unit-tested", not "works".

## Phase 2 — First real copy ✅

Verified against MySQL 8.4 (`tests/copy_integration.rs`):

- [x] All common column types copy losslessly (ints incl. `BIGINT UNSIGNED` max,
      `DECIMAL(30,10)`, float/double, utf8mb4, NUL bytes in blobs, JSON,
      `DATETIME(6)`, `TIMESTAMP(3)`, `TIME`, `YEAR`, `BIT`, `ENUM`, `SET`, NULLs)
- [x] Keyset paging over 5,000 rows with a composite primary key
- [x] Wide tables stay under the 65,535 placeholder limit
- [x] Parent filters propagate to children (single, nullable and composite FKs)
- [x] `replace` is atomic: a failure mid-copy leaves the destination untouched
- [x] `merge` upserts and keeps unrelated rows; auto-increment continues
- [x] Only shared columns are copied; missing destination table is rejected
- [x] Source == destination (same server + database) is refused
- [x] `replace` without `--force` is refused; dry-run writes nothing and reports exact counts

Fixed from Phase 1: unsigned/decimal/timestamp/JSON decoding, FK children
dangling after parent filters, no rollback, no snapshot, row-count estimate
crash on `BIGINT UNSIGNED`, lossy `preserve` placeholder, composite FKs.

Known limits (documented, not hidden): FK cycles and self-referencing tables are
rejected; spatial types are unsupported; non-InnoDB tables are not rolled back;
copying is sequential (one snapshot, one transaction), so there is no
concurrency setting.

## Phase 3 — Anonymization ✅

Verified against MySQL 8.4 (`tests/anonymize_integration.rs`) and unit tests
(`tests/transformations.rs`):

- [x] No original email, name, phone, password hash or date appears in any
      destination cell (scan of every cell against 1,800 source values)
- [x] Unique indexes hold (300 distinct emails and phones); 50,000 distinct
      inputs give 50,000 distinct outputs for `unique_phone` / `unique_email`
- [x] The same person gets one email in `users` and `orders` (identity is keyed
      by transform kind, not by column)
- [x] Same secret → same output across runs; different secret → different;
      no secret → random per run (never a built-in secret)
- [x] `random_date` honours `min` / `max`; bad bounds are rejected
- [x] Transforms that cannot fit (too long, wrong type, on the paging key) are
      rejected before any write; nothing is written
- [x] Database errors do not echo values (only the MySQL error number)
- [x] `anonymize: false` copies raw values

Changes from Phase 1: removed the hard-coded fallback secret, fixed
`unique_phone` collisions (10,000 values), removed the `deterministic` option
and the `fake` crate (HMAC-derived values from small built-in lists), flattened
column options (`dob: { transform: random_date, min: .. }`).

Known limits: client-side transforms hold raw values in memory for a batch;
`hash` needs a column of at least 64 characters.

## Phase 4 — Developer experience ✅

Verified against MySQL 8.4 (`tests/cli.rs`, `tests/copy_integration.rs`):

- [x] `--group` copies members plus their foreign-key parents only; `--tables`
      restricts the run; an excluded parent is an error, not a silent gap
- [x] CLI exit codes: config 3, safety 4, validation 6; `init` never overwrites
      without `--force`; production destination refused even with `--force`;
      `replace` without `--force` refused
- [x] Output contains neither connection passwords nor source values
- [x] Killing the process mid-copy (300,000 rows, killed at 400 ms of a ~3 s run)
      never leaves a partial destination
- [x] Per-table throughput in the output; plan shows exact row counts
- [x] Removed the unused `jobs` and `preserve_auto_increment` keys (they did
      nothing; ids are always preserved)

Initial throughput was measured as a baseline before any tuning.

Recovery: there are no automatic retries. A failed or interrupted run leaves the
destination exactly as it was (InnoDB); fix the cause and run again. Re-running
is safe because `replace` clears and reloads, and `merge` is an upsert.

## After Phase 4 — pipeline, CI, publishing ✅

- [x] Reader/writer pipeline (see ARCHITECTURE). 1M rows × 5 columns, release
      build, local Docker MySQL 8.4 (one machine, indicative only):

      | Case | Sequential | Pipelined |
      |---|---|---|
      | Plain copy | 287k rows/s | 323k rows/s |
      | 4 transforms (HMAC, unique tracking) | 188k rows/s | 266k rows/s |

- [x] Test that a mid-run read failure commits nothing (mutation-checked: the
      test fails if the commit rule is weakened)
- [x] GitHub Actions: fmt, clippy, tests against MySQL via docker compose, and
      an MSRV (1.94) check; actions pinned to commit SHAs; Dependabot
- [x] Release workflow publishes to crates.io from CI on a `v*` tag, in a
      protected `crates-io` environment. See `docs/RELEASING.md`
- [ ] First publish (needs the `CARGO_REGISTRY_TOKEN` environment secret)

## Open items

- Foreign-key cycles / self-referencing tables (rejected today)
- Spatial column types
- Structured filters instead of raw SQL `where`
- TLS options are whatever the URL specifies; not covered by tests
- Parallel table copy (would need one transaction per table, losing atomicity)
- CI workflow
