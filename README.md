# spansh-importer

Load the whole Elite Dangerous galaxy into your own PostgreSQL database.

[Spansh](https://spansh.co.uk/dumps) publishes daily dumps of everything players have
explored: about 200 million star systems, 600 million planets and stars, every station, market
price and faction. This tool turns those dumps into ordinary database tables you can query with
SQL. You can use them for route planning, trade analysis, exploration statistics, or to back your
own tools.

- **One command per step:** create the tables, import, finish.
- **No big download needed:** it reads the dump straight from Spansh's server. Point it at a local
  copy instead if you have one.
- **Safe to interrupt:** stop it any time (Ctrl-C, reboot, lost connection) and run the same command
  again to continue where it left off. Nothing is imported twice.
- **Nothing gets lost:** fields it doesn't recognise are kept, not dropped.
- **Modest hardware:** uses about 300 MB of RAM. The full galaxy takes about 3 hours on a
  desktop PC.

## What you need

- **PostgreSQL 15 or newer** on the same machine or reachable over the network, with an empty
  database for the import.
- **Disk space** for the database, depending on which dump you import:

| Dump | Download size | Contents | Database size | Import time |
|---|---:|---|---:|---:|
| `galaxy.json.gz` | 110 GB | everything | ~260 GB | ~3 h |
| `galaxy_populated.json.gz` | 4.3 GB | inhabited systems only, with all stations and markets | 12 GB | ~8 min |
| `galaxy_stations.json.gz` | 4.3 GB | systems that have stations | | |
| `galaxy_1month.json.gz` | 5.2 GB | systems updated in the last month | | |
| `galaxy_7days.json.gz` | 3 GB | systems updated in the last 7 days | | |
| `galaxy_1day.json.gz` | 1.1 GB | systems updated in the last day | | |

All dumps are at `https://downloads.spansh.co.uk/<name>`. Sizes and times were measured on a
16-thread desktop with an NVMe SSD. The full-galaxy database size is approximate.

## Install

Download the archive for your system from the [releases page](../../releases) and unpack it. It
contains the `spansh-importer` program, this README and the `db` folder.

To build from source instead, install [Rust](https://rustup.rs/) and run `cargo build --release`.
The program is then at `target/release/spansh-importer`.

## Import the galaxy

**1. Create a database** (once):

```sql
CREATE DATABASE elite;
```

Run this in `psql`, pgAdmin or any SQL tool. From a terminal, PostgreSQL's `createdb` tool does
the same: `createdb -U postgres elite`. It's in PostgreSQL's `bin` folder if it's not on your PATH.

**2. Tell the importer where it is.** With no settings, it connects to database `elite` on
`localhost:5432` as user `postgres`. Otherwise use any of the usual PostgreSQL ways:

- a connection string in `DATABASE_URL`, or `--database` on each command:

  ```sh
  export DATABASE_URL="postgres://user:password@host:5432/elite"     # Linux / macOS
  $env:DATABASE_URL = "postgres://user:password@host:5432/elite"     # Windows PowerShell
  ```

  The `key=value` form also works: `"host=db.example user=me dbname=elite sslmode=require"`.
- the standard variables `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`, `PGSSLMODE` and
  `PGSSLROOTCERT`. They fill in whatever the connection string leaves out.
- a password file: `~/.pgpass` on Linux and macOS, `%APPDATA%\postgresql\pgpass.conf` on Windows,
  or the file named in `PGPASSFILE`. This keeps the password out of commands and shell history.

**Encryption** works as in other PostgreSQL tools:

| `sslmode` | Meaning |
|---|---|
| `prefer` (default) | Encrypt if the server supports it. |
| `require` | Always encrypt. The server's certificate isn't checked, unless you also give `sslrootcert`. |
| `verify-ca` | Encrypt, and check the certificate against `sslrootcert`, or the system's trusted certificates. |
| `verify-full` | Also check that the certificate matches the host name. Use this for cloud databases. |
| `disable` | Never encrypt. |

Add it to the URL as `?sslmode=verify-full&sslrootcert=/path/ca.pem`. Cloud providers publish their
CA certificate for `sslrootcert`.

Special characters in a password must be percent-encoded in the URL form, for example `@` as `%40`.
In the `key=value` form, a backslash escapes the next character, as in other PostgreSQL tools.
Write Windows paths there with forward slashes (`sslrootcert='C:/certs/ca.pem'`) or doubled
backslashes.

**3. Run the three steps:**

```sh
spansh-importer init
spansh-importer ingest --source https://downloads.spansh.co.uk/galaxy.json.gz
spansh-importer finalize
```

- **`init`** creates the empty tables. It deletes any previous import in that database.
- **`ingest`** streams the dump into the tables. This is the long step.
- **`finalize`** checks the data for duplicates and broken references, then builds the indexes that
  make queries fast, a 3D index for "what's near here?" searches, and the `system_flags` table of
  route-planning flags. For the full galaxy this takes about 40 minutes to an hour.

To import a file you've already downloaded, give its path instead of the URL:
`--source D:\dumps\galaxy.json.gz`.

## While it runs

Each command shows a live status in the terminal:

```
ingest  [#######.............]  38.7%  45.8 GB / 118.5 GB  1h12m elapsed, ETA 1h54m
batch 2478, 66067109 systems (+0 skipped), 239595 MB JSON (55 MB/s), 166315 MB COPY, 0 issues
warnings 0  errors 0  log logs\ingest-20261030-080506.log
```

Every run also writes a log file in the `logs` folder, named after the command and its start time.
The log has a progress line every minute plus any warnings or errors, so you can check on a long
import later or from another machine. Use `--log-dir` to put logs elsewhere.

## If it stops

Run **exactly the same command again**. `ingest` skips everything that was already saved and
continues, and `finalize` skips the steps that already finished.

There are a few cases where the importer refuses to continue instead:

| Message | Why | What to do |
|---|---|---|
| `source changed since run N` | The dump file is different from the one the import started with. Spansh publishes a new dump every day around 06:40 UTC. | Run `init` and start over, or import a local copy you downloaded once. |
| `already imported this source completely` | This dump is already fully in the database. | Nothing; run `finalize` if you haven't. |
| `tables already hold data from another source` | The database holds a different import. | Run `init` first (deletes it). |

A dropped internet connection on its own is not a problem: the importer reconnects and carries on
from the exact byte, as long as Spansh hasn't replaced the file in the meantime.

## Updating to a newer dump

Imports aren't incremental. To move to a newer dump, run the three steps again. To keep using the
old data meanwhile, import into a second schema (`--schema spansh_new` on all three commands), then
swap the schemas when it's done:

``sql
ALTER SCHEMA spansh RENAME TO spansh_old;
ALTER SCHEMA spansh_new RENAME TO spansh;
DROP SCHEMA spansh_old CASCADE;
``

## Using the data

Everything is in the `spansh` schema, or the one you chose with `--schema`. Run
`SET search_path = spansh, public;` first, or prefix the table names.

Find a system:

```sql
SELECT name, x, y, z, population FROM system WHERE name = 'Sol';
```

Where to sell Gold for the best price:

```sql
SELECT st.name AS station, sy.name AS system, m.sell_price, m.demand
FROM market_commodity m
JOIN commodity c ON c.id = m.commodity_id
JOIN station st  ON st.id = m.station_id
JOIN system sy   ON sy.id64 = st.system_id64
WHERE c.name = 'Gold' AND m.demand > 0
ORDER BY m.sell_price DESC
LIMIT 10;
```

Landable bodies in a system, lowest gravity first:

```sql
SELECT b.name, t.name AS type, b.gravity, b.distance_to_arrival
FROM body b
JOIN system s ON s.id64 = b.system_id64
LEFT JOIN lookup t ON t.id = b.sub_type
WHERE s.name = 'Sol' AND b.is_landable
ORDER BY b.gravity;
```

The 10 systems nearest to Sol. This uses the 3D index, so write the distance exactly in this form:

```sql
SELECT name, cube(array[x, y, z]) <-> cube(array[0, 0, 0]) AS ly
FROM system
ORDER BY cube(array[x, y, z]) <-> cube(array[0, 0, 0])
LIMIT 10;
```

The nearest system with a fuel-scoopable arrival star, from a given position:

```sql
SELECT s.name, cube(array[s.x, s.y, s.z]) <-> cube(array[-78.6, -149.6, -340.5]) AS ly
FROM system s JOIN system_flags f ON f.id64 = s.id64
WHERE f.main_star_scoopable
ORDER BY cube(array[s.x, s.y, s.z]) <-> cube(array[-78.6, -149.6, -340.5])
LIMIT 1;
```

Stations that sell a ship:

```sql
SELECT st.name, sy.name AS system
FROM station st JOIN system sy ON sy.id64 = st.system_id64
WHERE st.ship_ids @> ARRAY(SELECT id FROM ship WHERE name = 'Python');
```

Some columns store names as small numbers to save space, for example `body.sub_type`,
`system.primary_economy` and `station.type`. Join them to `lookup` to get the text, as in the third example.

What's in each table:

| Table | Rows (full galaxy) | What it holds |
|---|---:|---|
| `system` | 203M | name, coordinates, allegiance, economy, population, Powerplay |
| `body` | 610M | stars and planets: orbits, physical data, atmosphere, materials, parents |
| `ring` | 95M | rings and asteroid belts |
| `body_signal`, `ring_signal` | 31M, 3.4M | biological, geological and other signals |
| `station` | 865k | stations, outposts, settlements and fleet carriers with their services |
| `market_commodity` | 105M | commodity prices, supply and demand per station |
| `station_economy` | 1.3M | station economies |
| `system_faction`, `faction_state` | 661k, 250k | minor factions and their states |
| `commodity`, `ship`, `module_variant` | | names of commodities, ships and outfitting modules |
| `system_flags` | | route planning per system: arrival star type, fuel-scoopable arrival star, scoopable star within 100 ls, neutron star nearby |
| `lookup` | | text for the numeric name columns |

[docs/field-mapping.md](docs/field-mapping.md) lists where every field from the dump ends up.

## Command reference

| Command | Purpose |
|---|---|
| `init` | Create empty tables (deletes any previous import). |
| `ingest --source <url or file>` | Import a dump; run again to resume. |
| `finalize` | Check the data and build indexes; run again to resume. |
| `survey <url or file> --out report.txt` | Profile a dump without importing it: every field, its types and values. No database needed. |

Options:

| Option | Default | Meaning |
|---|---|---|
| `--database` | `$DATABASE_URL`, then `PG*` variables, then `postgres@localhost:5432/elite` | Database to use (see step 2 above) |
| `--schema` | `, else `spansh` | Database schema for the tables. Use a second schema to build a fresh import next to the one in use. |
| `--log-dir` | `logs` | Where log files go |
| `--no-spatial` | | `finalize`: skip the 3D index on system coordinates |
| `--no-flags` | | `finalize`: skip the `system_flags` table |
| `--batch-mib` | `64` | `ingest`: how much data to save at a time. An interruption loses at most one batch. |
| `--max-systems` | | `ingest`: stop after this many systems, e.g. for a quick trial. Resumable. |
| `--issue-cap` | `100000` | `ingest`: how many data problems to store in `import_issue`; all are counted. |
| `--workers` | `8` | `survey`: threads to use |

## Faster imports

The defaults of a fresh PostgreSQL install work, but a large import is much faster with bulk-load
settings. [db/bulkload.conf](db/bulkload.conf) is the configuration used for the timings above.
Include it from your `postgresql.conf` and adjust the memory values to your machine.

## More documentation

- [docs/technical.md](docs/technical.md): how the importer works, resume details, log format,
  performance breakdown, development and testing.
- [docs/field-mapping.md](docs/field-mapping.md): where each field of the dump goes.
- [docs/research-notes.md](docs/research-notes.md): notes on the dump format and its quirks.

## License

MIT. See [LICENSE](LICENSE).

## Acknowledgements

The data comes from [Spansh](https://spansh.co.uk/), built from player-submitted
[EDDN](https://github.com/EDCD/EDDN) data. Elite Dangerous is a trademark of Frontier Developments
plc. This project is not affiliated with Spansh or Frontier.

The test fixture `tests/fixtures/spanshtools-100.json.gz` comes from
[Merovech/Elite.SpanshTools](https://github.com/Merovech/Elite.SpanshTools) (MIT).
