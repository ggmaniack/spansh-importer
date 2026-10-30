# spansh-importer

[![CI](https://github.com/ggmaniack/spansh-importer/actions/workflows/ci.yml/badge.svg)](https://github.com/ggmaniack/spansh-importer/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**The whole Elite Dangerous galaxy in your own PostgreSQL database.**

[Spansh](https://spansh.co.uk/dumps) publishes nightly dumps of everything players have explored:
about **200 million systems**, **600 million stars and planets**, every station, market and
faction. `spansh-importer` turns a dump into ordinary SQL tables you can query for route planning,
trade, exploration stats, or as the backend of your own tool.

- ⚡ **Streams straight from Spansh.** No 110 GB download first; a local file works too.
- ⏯️ **Stop and resume any time.** Ctrl-C, a reboot, a dropped connection: run the same command
  again and it picks up where it left off.
- 🧭 **Ready for route planning.** Includes a 3D index for "nearest system" searches and per-system
  flags: scoopable star, neutron star nearby.
- 🔒 **Lossless.** Fields it doesn't know are kept, never dropped.
- 🪶 **Light.** About 300 MB of RAM.

## Quick start

1. **Get the program** from the [releases page](../../releases), or build it with `cargo build --release`.
2. **Create a database** in PostgreSQL 15 or newer: `CREATE DATABASE elite;`
3. **Import:**

   ```sh
   spansh-importer init
   spansh-importer ingest --source https://downloads.spansh.co.uk/galaxy.json.gz
   spansh-importer finalize
   ```

| Step | What it does |
|---|---|
| `init` | Creates the empty tables. Deletes any previous import. |
| `ingest` | Streams the dump into the tables. |
| `finalize` | Checks the data, then builds indexes and route-planning flags. |

By default it connects to database `elite` on `localhost:5432` as user `postgres`. For another
server, a password, encryption or a different schema, see **[Configuration](docs/configuration.md)**.
It understands `DATABASE_URL`, the standard `PGHOST` / `PGUSER` / `PGPASSWORD` variables, password
files and `sslmode`.

## How long it takes

Measured on a 16-thread desktop with an NVMe SSD and the PostgreSQL settings in
[db/bulkload.conf](db/bulkload.conf):

| Dump | Download | `ingest` | Database size |
|---|---:|---:|---:|
| `galaxy.json.gz`, everything | 110 GB | 3 h | 284 GB |
| `galaxy_populated.json.gz`, inhabited systems | 4.3 GB | 8 min | 12 GB |

`finalize` on the full galaxy depends on which extras you keep:

| `finalize` | Time | What you get |
|---|---:|---|
| `--no-spatial --no-flags` | ~35 min | keys, data checks, indexes |
| `--no-spatial` | ~42 min | + `system_flags` route-planning table (7 min) |
| full (default) | ~2 h 25 min | + 3D index for distance searches (1 h 42 min) |

The 3D index is the slow part. Skip it with `--no-spatial` if you don't need nearest-system
searches. You can add it later by running `finalize` again without the switch: finished steps are
skipped.

Other dumps at `https://downloads.spansh.co.uk/`: `galaxy_stations` (systems with stations), and
`galaxy_1day`, `galaxy_7days`, `galaxy_1month` (systems updated in that window).

## While it runs

```
ingest  [#######.............]  38.7%  45.8 GB / 118.5 GB  1h12m elapsed, ETA 1h54m
batch 2478, 66067109 systems (+0 skipped), 239595 MB JSON (55 MB/s), 166315 MB COPY, 0 issues
warnings 0  errors 0  log logs\ingest-20261030-080506.log
```

Each run also writes a log file to `logs/`, with a progress line every minute plus any warnings and
errors.

## If it stops

**Run the same command again.** It skips what's already done.

It refuses to continue in three cases:

| Message | Meaning | Fix |
|---|---|---|
| `source changed since run N` | Spansh published a new dump since you started (daily, ~06:40 UTC). | `init` and start over, or import a downloaded copy. |
| `already imported this source completely` | Nothing left to do. | Run `finalize` if you haven't. |
| `tables already hold data from another source` | The schema holds a different import. | `init` first. |

## Querying

```sql
SET search_path = spansh, public;

-- Where to sell Gold
SELECT st.name AS station, sy.name AS system, m.sell_price, m.demand
FROM market_commodity m
JOIN commodity c ON c.id = m.commodity_id
JOIN station st  ON st.id = m.station_id
JOIN system sy   ON sy.id64 = st.system_id64
WHERE c.name = 'Gold' AND m.demand > 0
ORDER BY m.sell_price DESC LIMIT 10;

-- The 10 systems nearest to Sol (uses the 3D index; keep the expression exactly like this)
SELECT name, cube(array[x, y, z]) <-> cube(array[0, 0, 0]) AS ly
FROM system
ORDER BY cube(array[x, y, z]) <-> cube(array[0, 0, 0]) LIMIT 10;

-- Nearest system with a fuel-scoopable star, from a position
SELECT s.name
FROM system s JOIN system_flags f ON f.id64 = s.id64
WHERE f.main_star_scoopable
ORDER BY cube(array[s.x, s.y, s.z]) <-> cube(array[-78.6, -149.6, -340.5]) LIMIT 1;

-- Landable bodies in Sol, lowest gravity first
SELECT b.name, t.name AS type, b.gravity
FROM body b
JOIN system s ON s.id64 = b.system_id64
LEFT JOIN lookup t ON t.id = b.sub_type
WHERE s.name = 'Sol' AND b.is_landable
ORDER BY b.gravity;
```

Columns like `body.sub_type` or `station.type` store small numbers; join `lookup` for the text.

<details>
<summary><b>What's in each table</b></summary>

| Table | Rows (full galaxy) | Holds |
|---|---:|---|
| `system` | 203M | name, coordinates, allegiance, economy, population, Powerplay |
| `body` | 610M | stars and planets: orbits, physical data, atmosphere, materials |
| `ring` | 95M | rings and asteroid belts |
| `body_signal`, `ring_signal` | 31M, 3.4M | biological, geological and other signals |
| `station` | 865k | stations, settlements and fleet carriers with services, ships and modules |
| `market_commodity` | 105M | prices, supply and demand per station |
| `station_economy` | 1.3M | station economies |
| `system_faction`, `faction_state` | 661k, 250k | minor factions and their states |
| `system_flags` | | arrival star type, scoopable arrival star, scoopable star within 100 ls, neutron star nearby |
| `commodity`, `ship`, `module_variant` | | commodity, ship and module names |
| `lookup` | | text for the numeric name columns |

Every field of the dump and where it ends up: [docs/field-mapping.md](docs/field-mapping.md).

</details>

## Documentation

- **[Configuration](docs/configuration.md)**: connecting, encryption, schemas, all options, a full example.
- **[Technical reference](docs/technical.md)**: how it works, resume and log formats, performance, development.
- **[Field mapping](docs/field-mapping.md)**: where each field of the dump goes.

## License and credits

MIT. See [LICENSE](LICENSE).

Data by [Spansh](https://spansh.co.uk/), built from player-submitted [EDDN](https://github.com/EDCD/EDDN)
data. Elite Dangerous is a trademark of Frontier Developments plc. This project isn't affiliated
with Spansh or Frontier. The test fixture comes from
[Merovech/Elite.SpanshTools](https://github.com/Merovech/Elite.SpanshTools) (MIT).
