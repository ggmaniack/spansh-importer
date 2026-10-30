# Technical reference

How the importer works inside, for contributors and anyone tuning or debugging an import. For
everyday use, see the [README](../README.md).

## Pipeline

`ingest` is one worker reading the source and committing byte-sized batches:

1. **Read:** `src/source.rs` opens a local file or an HTTP(S) URL and decompresses `.gz` on the fly
   with `MultiGzDecoder`. A counting wrapper reports compressed bytes read for the progress bar.
2. **Frame:** the dump is `[`, one `{...},` system per line, then `]`. `system_bytes` strips that
   framing.
3. **Parse:** `serde_json` with `float_roundtrip`, so every double reads back exactly as written.
4. **Project:** `src/project.rs` turns one system into rows for up to 13 tables. Every source key is
   either mapped to a column or child row, or copied into the owning entity's `extra` JSONB. See
   [field-mapping.md](field-mapping.md).
5. **Encode:** `src/pgcopy.rs` writes PostgreSQL's binary COPY format directly into one buffer per
   table.
6. **Commit:** when the buffers reach `--batch-mib` (64 MiB by default), one transaction COPYs every
   table and inserts the batch's system range into `import_batch`.

Enum-like strings (economy, body type, ...) become `SMALLINT` ids in one `lookup` table. Catalogue
entities (`commodity`, `ship`, `module_variant`) are keyed by Frontier id. New values are inserted
and committed on first sight through a separate connection, so a batch only ever references rows
that are already committed.

## Correctness rules

- **Nothing is dropped.** Unknown keys, values of the wrong type and conflicting catalogue data go to
  `extra`, and a row in `import_issue` describes them. That table is capped by `--issue-cap`; every
  issue is still counted.
- **id64** is a `u64` stored as `BIGINT` by bit reinterpretation. Body `id64` equals
  `system_id64 | body_id << 55`, which was verified on all 610M bodies.
- **Timestamps** accept `YYYY-MM-DD[T ]HH:MM:SS[.f{1,6}](Z|+00|+00:00)`. Anything else is kept in
  `extra`.
- **Composition columns** cover the closed sets seen in the full galaxy: 3 solid components, 13
  atmosphere gases and 25 materials. A new component would go to `extra`.

## Resume

### ingest

Every run records a fingerprint of its source in `import_run`:

- **HTTP:** `Content-Length`, `ETag`, `Last-Modified`, plus the gzip trailer (CRC32 and
  uncompressed size mod 2^32), read with a `Range: bytes=-8` request.
- **Local file:** canonical path, size, mtime, plus the gzip trailer.

Each batch's `import_batch` row is committed in the same transaction as its data, so the
checkpoint can never disagree with the tables. On restart the importer re-reads the source from the
beginning and skips systems whose ordinal falls in a committed range.

| Situation | Result |
|---|---|
| Same source, unchanged | Committed ranges are skipped and the import continues. |
| Same source, fingerprint differs | Refused: "source changed". |
| Previous run finished (`done`) | Refused: "already imported". |
| Tables hold data from a different source | Refused: "run `init` first". |

When an HTTP connection drops, it is resumed in-stream. The importer re-requests the remaining bytes
with `Range` and `If-Range: <ETag>`:
- **206 Partial Content:** the stream continues from the exact byte.
- **200 OK:** the file was replaced on the server, so the run aborts instead of splicing two
  versions together.

It retries up to 20 times in a row, backing off up to 60 s between attempts.

### finalize

`db/migrations/002_constraints.sql` runs one statement at a time, each in its own transaction:

1. primary keys (building the unique index is the duplicate check);
2. foreign keys added `NOT VALID` (instant: enforced for new writes only);
3. `VALIDATE CONSTRAINT` for each foreign key, which checks the existing rows under a lock that
   still allows reads and writes;
4. secondary indexes (`CREATE INDEX IF NOT EXISTS`);
5. `ANALYZE`.

Every constraint and index has an explicit name. Before each statement, `finalize` skips it if:
- its constraint already exists (`ADD CONSTRAINT`); or
- its constraint is already validated (`VALIDATE CONSTRAINT`).

Re-running `finalize` after a failure or Ctrl-C therefore continues at the first unfinished step. A
failed check stops with the offending constraint's name; the steps before it stay committed.

Lookup references are deliberately not foreign keys. About 30 columns reference `lookup`, and
validating each would cost a full table scan. The integration tests check those references
instead.

## Progress display and logs

`src/report.rs` is shared by all commands.

- **Terminal:** a three-line status block is redrawn every 500 ms with ANSI cursor movement. VT
  processing is switched on explicitly for Windows consoles. When stderr is not a terminal, the
  progress line is printed once a minute instead.
- **Progress source:**
  - `ingest` and `survey`: compressed bytes read against the file size (`Content-Length` or the
    local file size);
  - `finalize`: steps done, plus `pg_stat_progress_create_index`, polled every 2 s on a second
    connection.
- **Log file:** `<log-dir>/<command>-<YYYYMMDD-HHMMSS>.log`, timestamped in UTC. A run starting in
  the same second as another gets a `-2`, `-3`, ... suffix. Each line is logfmt with four fixed
  fields: `ts`, `level` (`INFO` / `WARN` / `ERROR`), `event` and `msg`. `msg` is quoted with `\"`,
  `\\`, `\n`, `\r` and `\t` escapes.
- **Events:**
  - `start`;
  - `progress`, once a minute;
  - `step`, each `finalize` statement with its duration;
  - `resumed`, finalize steps skipped as already done;
  - `issue`, a data problem;
  - `reconnect` / `reconnect_failed`;
  - `error`;
  - `summary`;
  - `done` / `failed`.
- **Rate limit:** each `WARN` or `ERROR` event name is written at most 20 times per run. Further
  occurrences are counted, and a `suppressed` line with the count is written after each progress
  line. `INFO` events come from bounded code paths and are always written.

## Performance

Full `galaxy.json.gz` (2026-10-30), streamed over HTTP, one worker, on a 16-thread desktop with an
NVMe SSD and PostgreSQL 18:

| | |
|---|---|
| Wall time | 2 h 58 min (10,698 s) |
| Throughput | 58 MB/s of JSON, 19k systems/s, 98k rows/s |
| Rows written | 1.05 billion |
| Peak memory | 315 MB |
| Database size | 223 GB before `finalize` |
| Time split | read + gunzip 16%, JSON parse 32%, projection 28%, COPY + commit 24% |

Parsing and projection are about 60% of the time and run on one thread. They are the target for
parallel workers.

PostgreSQL settings for bulk loading are in [db/bulkload.conf](../db/bulkload.conf). The ones that
matter most:
- `wal_level = minimal`;
- `max_wal_size = 32GB`;
- a large `maintenance_work_mem`, for `finalize`.

Durability (`fsync`, `synchronous_commit`) stays on.

## Development

```sh
cargo clippy --all-targets -- -D warnings
cargo test                      # unit tests + integration tests
```

The integration tests in `tests/ingest.rs` need a PostgreSQL server at `$TEST_PG`. The default is
`postgres://postgres@localhost:5432` with trust authentication. Each test creates its own
`spansh_test_*` database. They cover:
- the row counts the 100-system fixture should produce;
- a field-by-field round-trip of systems, bodies, stations, compositions, parents and modules;
- resume without duplicates;
- rollback of a failed batch;
- refusal to resume a changed source;
- `finalize` resuming after a failed foreign key check.

CI (`.github/workflows/ci.yml`) runs clippy and all tests against a `postgres:18` service container.
Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds Linux x86_64, Windows x86_64
and macOS arm64 binaries and publishes them as a GitHub release.

On Windows, [db/pg.ps1](../db/pg.ps1) wraps `pg_ctl` for a native PostgreSQL install with its data
in `F:\pgdata`. Adjust the paths for your machine.

| Path | Contents |
|---|---|
| `src/main.rs` | command line |
| `src/source.rs` | file and HTTP streaming, reconnect, source fingerprint |
| `src/project.rs` | JSON to rows, lookup and catalogue dictionaries, issue log |
| `src/pgcopy.rs` | binary COPY encoder |
| `src/ingest.rs` | batching, transactions, resume, `init` and `finalize` |
| `src/report.rs` | progress display and log files |
| `src/survey.rs` | parallel data profiler |
| `db/migrations/` | schema (load phase) and constraints (after load) |
| `docs/field-mapping.md` | where every source field goes |
| `docs/research-notes.md` | dump format, data quirks, design decisions |
| `docs/survey/` | survey reports of each Spansh dump, including the full galaxy |
