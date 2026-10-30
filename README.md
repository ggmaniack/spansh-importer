# spansh-importer

Streams the [Spansh](https://spansh.co.uk/dumps) Elite Dangerous galaxy dump into normalized
PostgreSQL tables. The full galaxy (`galaxy.json.gz`: 110 GB compressed, about 620 GB of JSON, 203M
systems, 610M bodies) loads in about 3 hours on one desktop. The importer uses about 300 MB of RAM and
needs no local copy of the dump.

- **Streaming:** reads a local file or an HTTP(S) URL. `.gz` is decompressed on the fly, and nothing
  is staged on disk.
- **Fast:** each table is written with hand-encoded binary `COPY`. Keys and indexes are built
  once, after the load.
- **Lossless:** every source field ends up in exactly one place:
  - a typed column or child row; or
  - the owning entity's `extra` JSONB column, for unknown fields and values of the wrong type,
    each logged to `import_issue`.
- **Resumable:** batches commit atomically together with their checkpoint, so an interrupted run
  continues exactly where it stopped, with no duplicates.
  - If the source file changed in the meantime, it refuses to resume.
  - Over HTTP, a dropped connection is resumed in-stream with `Range` + `If-Range`.
- **Bounded memory:** batches are capped by encoded size (`--batch-mib`), not by row count.

## Quick start

Requirements: PostgreSQL 15 or newer (developed on 18) and a database to load into. To build from
source you also need a Rust toolchain (edition 2024, Rust 1.85 or newer).

```sh
cargo build --release
export DATABASE_URL=postgres://postgres@localhost:5432/elite

spansh-importer init                      # (re)create the empty `spansh` schema
spansh-importer ingest --source https://downloads.spansh.co.uk/galaxy.json.gz
spansh-importer finalize                  # primary keys, foreign keys, indexes, ANALYZE
```

Prebuilt binaries for Linux, Windows and macOS are attached to each
[GitHub release](../../releases).

## Commands

| Command | What it does |
|---|---|
| `init` | Drops and recreates schema `spansh` from `db/migrations/001_schema.sql` (embedded in the binary). **Destroys any previous import.** |
| `ingest --source <path\|url>` | Streams a dump into the tables. Re-running the same command resumes an interrupted run. |
| `finalize` | Runs `002_constraints.sql` in one transaction: primary keys (which also prove there are no duplicates), foreign keys, secondary indexes, `ANALYZE`. |
| `survey <path\|url> --out report.txt` | Read-only data profiling. For every JSON path it reports types, ranges, distinct values and duplicate ids. It doesn't need a database. |

`ingest` options:

| Option | Default | Meaning |
|---|---|---|
| `--database` | `$DATABASE_URL`, else `postgres://postgres@localhost:5432/elite` | Target database |
| `--batch-mib` | `64` | Commit after this many MiB of encoded COPY data. One batch is the unit of resume. |
| `--max-systems` | | Stop after N systems. The run is marked `stopped` and can be resumed later. |
| `--issue-cap` | `100000` | Maximum `import_issue` rows stored per run. Every issue is still counted. |

Any Spansh dump with the one-system-per-line layout works: `galaxy.json.gz`, `galaxy_1day`,
`galaxy_7days`, `galaxy_1month`, `galaxy_populated`, `galaxy_stations`.

## Resume and source identity

Every run records a fingerprint of its source in `import_run`:

- **HTTP:** `Content-Length`, `ETag`, `Last-Modified`, plus the gzip trailer (CRC32 and size).
- **Local file:** canonical path, size, mtime, plus the gzip trailer.

Each committed batch inserts its system range into `import_batch` in the same transaction as its
data. On restart:

| Situation | Result |
|---|---|
| Same source, unchanged | Committed ranges are skipped and the import continues. |
| Same source, fingerprint differs | Refused. Run `init` and start over. |
| Previous run finished (`done`) | Refused, so a dump is never imported twice. |
| Tables hold data from a different source | Refused. Run `init` first. |

Spansh rebuilds the galaxy dump once a day, around 06:40 UTC. Within the same download, a dropped
HTTP connection resumes from the exact byte, and a file replaced mid-download aborts instead of
splicing two versions together. To resume across a rebuild, download the dump and import the
local file instead.

## Schema

All tables live in schema `spansh`. See [docs/field-mapping.md](docs/field-mapping.md) for where each
source field goes.

| Table | Rows (full galaxy, 2026-10-30) | Contents |
|---|---:|---|
| `system` | 203,339,502 | coordinates, allegiance, economy, population, powers, timestamps |
| `body` | 609,790,151 | orbital and physical data, parents (arrays), composition (41 percent columns) |
| `ring` / `ring_signal` | 95.2M / 3.4M | rings and belts, with their signals |
| `body_signal` | 31.5M | biological, geological and other signal counts |
| `station` | 864,950 | stations and fleet carriers, services, ship and module-variant id arrays |
| `station_economy` | 1.3M | economy shares |
| `market_commodity` | 104.5M | buy/sell prices, demand, supply |
| `system_faction` / `faction_state` | 661k / 250k | factions and their active, pending and recovering states |
| `power_conflict`, `thargoid_war` | | Powerplay conflict progress, Thargoid war state |
| `lookup` | | enum-like strings (economy, government, body type, ...) as `SMALLINT` ids |
| `commodity`, `ship`, `module_variant` | | Frontier catalogue entities |
| `import_run`, `import_batch`, `import_issue` | | run fingerprints, resume checkpoints, logged problems |

Body `id64` equals `system_id64 | body_id << 55`, which was verified on all 610M bodies.

## Performance

Full `galaxy.json.gz`, streamed over HTTP, single worker, on a 16-thread desktop with an NVMe SSD:

| | |
|---|---|
| Wall time | 2 h 58 min (10,698 s) |
| Throughput | 58 MB/s of JSON, 19k systems/s, 98k rows/s |
| Rows written | 1.05 billion |
| Peak memory | 315 MB |
| Database size | 223 GB before `finalize` |
| Time split | read + gunzip 16%, JSON parse 32%, projection 28%, COPY + commit 24% |

PostgreSQL settings for bulk loading are in [db/bulkload.conf](db/bulkload.conf). The settings that
matter most:
- `wal_level = minimal`: COPY into a table created in the same transaction skips WAL.
- `max_wal_size = 32GB`
- a large `maintenance_work_mem` for `finalize`

Durability (`fsync`, `synchronous_commit`) stays on.

## Development

```sh
cargo test                       # unit tests + integration tests
TEST_PG=postgres://postgres@localhost:5432 cargo test
```

The integration tests in `tests/ingest.rs` need a PostgreSQL server (`$TEST_PG`, which defaults to
`postgres://postgres@localhost:5432` with trust auth). Each test creates and drops its own
`spansh_test_*` database. They cover:
- the row counts the 100-system fixture should produce;
- a field-by-field round-trip of systems, bodies and stations;
- resume without duplicates;
- rollback of a failed batch;
- refusal to resume a changed source.

On Windows, [db/pg.ps1](db/pg.ps1) wraps `pg_ctl` for a native PostgreSQL install whose data
directory is `F:\pgdata` (`.\db\pg.ps1 start|stop|status`). Adjust the paths for your machine.

Repository layout:

| Path | Contents |
|---|---|
| `src/main.rs` | CLI |
| `src/source.rs` | file/HTTP streaming, reconnect, source fingerprint |
| `src/project.rs` | JSON to row projection, lookup and catalogue dictionaries, issue log |
| `src/pgcopy.rs` | binary COPY encoder |
| `src/ingest.rs` | batching, transactions, resume |
| `src/survey.rs` | parallel data profiler |
| `db/migrations/` | schema (load phase) and constraints (after load) |
| `docs/research-notes.md` | dump format, data quirks, design decisions |
| `docs/survey/` | survey reports of each Spansh dump, including the full galaxy |

## Data and acknowledgements

The data comes from [Spansh](https://spansh.co.uk/), built from player-submitted EDDN journal
data. Elite Dangerous is a trademark of Frontier Developments plc. This project is not affiliated
with Spansh or Frontier.

The test fixture `tests/fixtures/spanshtools-100.json.gz` comes from
[Merovech/Elite.SpanshTools](https://github.com/Merovech/Elite.SpanshTools) (MIT). See
[tests/fixtures/README.md](tests/fixtures/README.md).
