# Configuration reference

Default file: `.dumpling.yml` (`dumpling init` writes a commented one; see also
`.dumpling.example.yml`). Unknown keys are errors. `${VAR}` is replaced from the
environment, and a missing variable is an error.

**Precedence:** CLI flags (`--tables`, `--group`, `--env`, `--force`,
`--dry-run`) > the selected `environments:` profile > top-level `source` /
`destination` / `mode` > `defaults`. Table rules apply on top of defaults.

## Keys

| Key | Meaning |
|---|---|
| `version` | Must be `1` |
| `defaults.batch_size` | Rows per page and per insert (default 2000) |
| `defaults.jobs` | Accepted, currently unused (copying is sequential) |
| `defaults.anonymize` | `false` copies raw values (default `true`) |
| `defaults.safe_destination` | Require `allow_destination: true` (default `true`) |
| `defaults.transform_secret` | HMAC secret. Prefer `DUMPLING_TRANSFORM_SECRET` |
| `connections.<name>.url` | `mysql://` URL, usually `${ENV}` |
| `connections.<name>.production` | Writes are always refused |
| `connections.<name>.allow_destination` | Explicit opt-in to write here |
| `source`, `destination` | Connection names |
| `environments.<name>` | `{source, destination}` pair for `--env` |
| `mode` | `merge` (default) or `replace` |
| `tables.<name>.where` | SQL boolean expression (trusted admin input) |
| `tables.<name>.columns.<col>` | `{transform: <name>, ...options}` |
| `exclude` | Tables never copied |
| `groups.<name>` | Table list for `--group` |

If `tables:` is set, only those tables (plus their foreign-key parents) are
copied. Otherwise every table not in `exclude` is copied.

## Modes

- `merge`: `INSERT .. ON DUPLICATE KEY UPDATE`. Existing rows with the same key
  are updated, other rows are kept. A conflict on a *non-primary* unique key
  updates the conflicting row.
- `replace`: `DELETE` the selected tables, then `INSERT`. Needs `--force`, and
  non-interactive runs also need `DUMPLING_ALLOW_REPLACE=1`. A unique-key
  collision is an error rather than a silent overwrite.

## Filters

`where` is inserted into `SELECT .. WHERE (..)` as written, so keep it a single
boolean expression. Filters containing `;`, comments or statement keywords
(`drop`, `insert`, ...) are rejected. A filter on a parent table also restricts
its children, so the copy never contains a dangling foreign key.

## Transforms

| Name | Output | Notes |
|---|---|---|
| `null` | NULL | Not allowed on `NOT NULL` columns. The column is never read |
| `constant` | `value:` | The column is never read |
| `preserve` | original | Explicit keep; a warning is printed |
| `fake_email` / `unique_email` | `user<hex>@example.invalid` | Reserved TLD |
| `fake_first_name` / `fake_last_name` | name from a built-in list | Repeats are expected |
| `fake_phone` / `unique_phone` | `+1-555-<7 digits>` | Fictional 555 exchange |
| `fake_address` | `<n> <street> St, <city>, ST <zip>` | |
| `random_date` | `YYYY-MM-DD` | Options `min`, `max` (default 1950-01-01 to 2005-12-31) |
| `hash` | 64 hex characters (HMAC-SHA256) | Pseudonymization, not anonymization |

How replacements are chosen:

- Every replacement is `HMAC(secret, kind | original value)`. The same source
  value gets the same replacement everywhere (for example `users.email` and
  `orders.contact_email`), so joins by value still work.
- `unique_*` transforms additionally never give two different inputs the same
  output. They keep one digest and one output per distinct value in memory for
  the run. `fake_*` transforms do not track this.
- With `DUMPLING_TRANSFORM_SECRET` set, output is stable between runs. Without
  it a random secret is generated per run (and a warning is logged), so output
  differs each run. Dumpling never falls back to a built-in secret.
- SQL `NULL` stays `NULL`.
- Transforms are checked before anything is written: the column must be text
  (or date for `random_date`) and long enough for the output. Key columns used
  for paging (primary key) cannot be transformed.

### What this does and does not protect

- `null` and `constant` columns are never read from the source.
- Every other transform reads the real value into Dumpling's memory for the
  current batch, then discards it. Nothing is written to disk or logged.
- Pseudonyms are only as strong as the secret. Anyone holding the secret and a
  guess of an original value can confirm the guess. Keep it out of the repo.
- Columns you do not configure are copied as-is. Run `dumpling scan` and review.
- Free-text columns (notes, comments) can contain personal data. Transform or
  exclude them.

## Production safety

Checked in order. `--force` only affects the last step.

1. A destination with `production: true` is never written.
2. Hosts that look like production (RDS, Cloud SQL, Azure, names containing
   `prod`, ...) are refused. This is a backstop, not the main protection.
3. With `safe_destination` (default), the destination needs
   `allow_destination: true`.
4. After connecting, source and destination must be different databases
   (server UUID + schema), which catches aliases and tunnels.
5. `mode: replace` needs `--force` (and `DUMPLING_ALLOW_REPLACE=1` when not
   interactive).

Dry-run never writes. Error messages never include passwords, and database
errors are reduced to the MySQL error code because MySQL messages can quote row
values (use `-vv` for the full text).

## Permissions

- Source: `SELECT` on the copied tables (a read-only account is ideal).
- Destination: `SELECT, INSERT, UPDATE, DELETE` on the copied tables.

Use TLS for remote databases (`?ssl-mode=required` in the URL). Certificate
verification is not disabled by default.

## Performance

- `batch_size` of 1000 to 5000 works well. Inserts are split automatically to
  stay under MySQL's 65,535 placeholder limit.
- Tables are paged by primary key (or a `NOT NULL` unique index).
- Keep `where` filters indexed. The plan runs an exact `COUNT(*)` per table.

## Limitations

- The destination schema must exist. Only columns present on both sides are
  copied; others are skipped with a warning.
- Foreign-key cycles and self-referencing tables are rejected.
- Spatial column types are unsupported. Tables with no primary key or `NOT NULL`
  unique index are unsupported.
- The run is one destination transaction, atomic for InnoDB. DDL and
  non-transactional engines cannot be rolled back.
- All tables are read from one consistent snapshot; a very long copy keeps
  that snapshot (and its undo log) open on the source.
- Zero dates (`0000-00-00`) fail under strict `sql_mode`.
