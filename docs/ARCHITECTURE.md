# Architecture

Dumpling copies selected tables from one MySQL database to another and
anonymizes configured columns on the way. It is deliberately small: it follows
the shape of [pgsync](https://github.com/ankane/pgsync) (config file, destination
schema must already exist, only shared columns are copied, an explicit
"is this destination safe?" gate) and adds anonymization and FK-aware filtering.

## Pipeline

```
config ──► resolve ──► safety gate ──► connect ──► plan ──► copy
 (YAML)   (profile,     (src/safety)   (src/db)   (src/planner)  (src/copy)
           CLI flags)
```

`src/sync.rs::run_sync` runs the whole pipeline; both the CLI and the
integration tests call it.

| Module | Responsibility |
|---|---|
| `config.rs` | Parse YAML, `${ENV}` interpolation, reject unknown keys, resolve profile / CLI overrides |
| `safety.rs` | Decide whether the destination may be written (see below) |
| `db/mysql.rs` | Pool creation; sessions pinned to `time_zone = '+00:00'` |
| `db/introspect.rs` | Tables, columns, PK / unique indexes, foreign keys (composite FKs grouped) |
| `db/value.rs` | Lossless column transport: how each MySQL type is read and bound back |
| `db/writer.rs` | Chunked `INSERT` / upsert, `DELETE`, redaction of DB errors |
| `planner.rs` | Table selection, FK dependency expansion, load order, effective filters, validation |
| `copy.rs` | Snapshot read → transform → transactional write |
| `transform/` | Registry and built-in transformations |
| `progress.rs` | Terminal output |

## Planning

1. **Selection**: `--group` > `--tables` > configured `tables:` > every table;
   then `exclude:` is removed.
2. **Dependencies**: parents of selected tables are added automatically
   (a parent that is excluded is an error, never silently skipped).
3. **Load order**: topological sort, parents first. FK cycles and
   self-references are **rejected** with a clear error.
4. **Effective filters**: a table's `where` is combined with a semi-join for each
   *restricted* parent, so children never reference parent rows that were
   filtered out:

   ```sql
   -- users: status = 'active'
   SELECT ... FROM orders
   WHERE (`user_id`) IN (SELECT `id` FROM `users` WHERE (status = 'active'))
   ```

   Nullable FK columns are exempt when NULL. Composite FKs use row tuples.
   This runs entirely in the source database.

## Copying

* **Source**: one connection in `START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY`.
  All tables are read as of the same instant, and Dumpling cannot write to the
  source.
* **Destination**: one transaction for the whole run. `mode: replace` does
  `DELETE` (children first), then loads parents first. Any error rolls
  everything back. This is atomic for InnoDB only; DDL and non-transactional
  engines (MyISAM) cannot be rolled back.
* **Paging**: keyset pagination on the primary key (or the first unique index
  whose columns are all `NOT NULL`): `WHERE (k1,k2) > (?,?) ORDER BY k1,k2 LIMIT n`.
  Memory is bounded by `batch_size`. Key columns cannot be transformed.
* **Inserts** are multi-row and split so a statement never exceeds MySQL's
  65,535 placeholder limit. `replace` uses plain `INSERT` (a unique-key
  collision is an error); `merge` uses `ON DUPLICATE KEY UPDATE`.
* **Shared columns only**: columns missing on the destination are skipped with a
  warning, as in pgsync. Destination-only columns get their defaults.
* **Auto-increment** values are copied explicitly; MySQL advances the counter
  past the largest copied id.

### Type transport (`db/value.rs`)

| MySQL type | Read as | Why |
|---|---|---|
| `TINYINT`..`BIGINT` (+`UNSIGNED`) | `i64` / `u64` | exact |
| `FLOAT` / `DOUBLE` | `f32` / `f64` | exact round trip |
| `CHAR`, `VARCHAR`, `*TEXT` | `String` | |
| `BINARY`, `VARBINARY`, `*BLOB` | `Vec<u8>` | |
| `DECIMAL`, `DATE`, `DATETIME`, `TIMESTAMP`, `TIME`, `JSON`, `ENUM`, `SET` | `CAST(.. AS CHAR)` text, bound back as text | a Rust float/date type would be lossy; MySQL parses the text exactly |
| `YEAR`, `BIT` | `CAST(.. AS UNSIGNED)` | |
| spatial types | unsupported (clear error) | |

## Safety model (`safety.rs`, `sync.rs`)

Checked in this order; `--force` only unlocks the last step.

1. `production: true` on the destination connection → refuse, always.
2. Host classification (loopback / well-known local names vs. `rds.amazonaws.com`,
   `prod`, ...). Production-looking hosts → refuse. This is a backstop only.
3. `safe_destination` (default on) requires `allow_destination: true` on the
   destination connection.
4. After connecting: the source and destination must not be the same database
   (compares `@@server_uuid` + `DATABASE()`, so aliases and tunnels are caught).
5. `mode: replace` needs `--force`; non-interactive runs also need
   `DUMPLING_ALLOW_REPLACE=1`.

Dry-run builds the plan and runs read-only counts; it never opens a destination
transaction.

## Privacy boundary

* `null` and `constant` columns are **never selected** from the source.
* Every other transform reads the raw value into process memory for the current
  batch, transforms it, and writes the result. Nothing is persisted or logged.
* Database error messages can embed row values (`Duplicate entry '...'`), so
  insert/read errors report only the MySQL error code; the full text is logged
  at `-vv`.

See `docs/PROGRESS.md` for what is verified and what is still open.
