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
`jobs` is accepted but unused (copy is sequential to keep one snapshot and one
transaction).

## Phase 3 — Anonymization ⏳

Transforms exist but are **not yet verified end to end**. Known defects to fix:

- [ ] Missing `DUMPLING_TRANSFORM_SECRET` falls back to a hard-coded public
      secret (pseudonyms would be reversible by dictionary). Use a random
      per-run secret instead.
- [ ] `unique_phone` has only 10,000 values and collides quickly.
- [ ] Identity is scoped per `table.column`; add an explicit shared identity so
      the same email maps consistently across tables.
- [ ] Transform output must fit the column (length, type) — validate at plan time.
- [ ] Tests: no source value survives in the destination; uniqueness holds.

## Phase 4 — Developer experience ⏳

- [ ] Progress output with throughput
- [ ] Groups and `--tables` verified end to end
- [ ] CLI end-to-end tests (exit codes)
- [ ] Remove or implement `jobs`
