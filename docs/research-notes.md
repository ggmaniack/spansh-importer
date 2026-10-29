# Phase 1 research notes

Sources (cloned to `E:\scratch\refs`, not part of this repo):
- Official schema: https://docs.spansh.co.uk/galaxy.schema.json (draft-04, fetched 2026-10-29)
- SBeausoleil/Spansh-Ingester (Rust, per-system upserts; 21-table schema, sample.json.gz)
- Merovech/Elite.SpanshTools (.NET streaming parser; `benchmarks/data.json` is a real-format extract)

## Dump format
- Real dumps: `[` on line 1, then one system per line, indented, with a trailing `,` (except the last), then `]`.
  Confirmed in `Elite.SpanshTools/benchmarks/data.json`; the schema also says "one system per line".
- The Spansh-Ingester `sample.json(.gz)` is **re-pretty-printed** (34,799 lines, multi-line objects), so it does
  not show the real line layout. Framer must not assume newlines alone, or the sample must be re-minified per line.
- Record size varies hugely: Sol is 1.87 MB on one line, deep-space systems ~150 bytes.
- Reported full `galaxy.json`: 481 GB / 161,064,963 systems (SpanshTools README); brief says ~577 GB now.
  SpanshTools parsed it in 41m27s (~65k systems/s, ~195 MB/s) - a useful single-threaded reference point.

## Value formats
- Timestamps look like `2025-05-27 17:09:48+00` (space separator, `+00` offset) - not RFC 3339; needs a custom parser.
- `parents`: array of single-key objects, e.g. `[{"Star":0},{"Null":1}]`; key is the parent kind.
- `timestamps` (system and body): free-form `{field: timestamp}` map.
- `atmosphereComposition`, `solidComposition`, `materials`, `economies`, `signals.signals`: `{name: number}` maps.
- Stations appear both at system level (`stations`) and nested under bodies (`bodies[].stations`).
- `controllingFaction` is a full faction object at system level, but a plain name string on stations.

## Known schema deviations (from the reference projects)
- `parents` uses kind `Ring`, not listed in the schema.
- `spectralClass` and `luminosity` contain values outside their declared enums.
- Schema lists allegiance `Frontline Solutions` and station state `DamagedHuman`; Spansh-Ingester's enums
  disagree (`FrontlineSolutions`, no `DamagedHuman`), so enum sets drift over time.
- Commodity, module and ship name lists are long and change with game updates -> not suitable for hard PG enums.

## What not to copy from Spansh-Ingester
- **Silently drops stations**: fleet carriers, mega ships, settlements, and anything without a controlling faction.
  This violates our no-silent-drop rule (carriers are a large share of stations).
- Truncates timestamps to seconds and discards the offset; stores `timestamp without time zone`.
- `NUMERIC` for every float and `NUMERIC(20,0)` for id64: slow to encode, large on disk.
- One transaction per system with `ON CONFLICT` upserts.
- Requires PostGIS for no visible reason; duplicate `idx_allegiance` statement.

## Survey: galaxy_populated.json.gz (2026-10-29)
Full report: `docs/survey/galaxy_populated.txt`. 4.6 GB gz -> 30.2 GB JSON, 154,966 systems, 63 s with
12 workers (477 MB/s decompressed; likely the single-threaded gzip ceiling - to be measured).

- 0 parse errors. 0 duplicate system id64, station id (853,745), body id64 (2.75M), or (system, bodyId).
- Largest system line: 11.4 MB (HIP 5807). Batching must cope with single records of >10 MB.
- **Row volume is dominated by markets and outfitting**: 104M commodity rows, 71.5M outfitting module rows,
  2.6M shipyard rows, 12.9M station service entries, 15M body material entries, 5.3M parent links,
  2.75M bodies, 854k stations (527k on bodies, 327k in space), 651k factions.
- Commodity: 411 distinct id/name/symbol; module: 1,216 ids, 1,198 symbols, 155 names; ship: 48.
  Name, symbol, category, class, rating, ship look like attributes of the numeric id -> lookup tables,
  with a consistency check at import time (the moduleId/symbol counts differ, so it is not a strict 1:1).
- Timestamps come in several shapes: `2026-06-29T23:03:36Z`, with 1-6 fractional digits
  (`...29.53Z`, `...43.462365Z`), and in older extracts `2025-05-27 17:09:48+00`. Microseconds fit TIMESTAMPTZ exactly.
- Ranges violate the schema's documented bounds: `orbitalInclination` -198..535, negative `orbitalPeriod`
  and `rotationalPeriod`, `surfaceTemperature` up to 9.85e8. -> no range CHECK constraints.
- Integer widths: `bodyId` <= 217 and `parents` ids <= 212 (SMALLINT); `demand` hits 2,147,483,647 (i32 max,
  INTEGER fits, but suggests clamping at the source); prices <= 60.8M; `population` <= 3.16e10 (BIGINT);
  station `id` <= 4.4e9 (BIGINT); system id64 <= 3.6e14 here, body/ring id64 up to 7.85e18 (still < 2^63).
- `powerStateReinforcement`/`Undermining` are integers in practice; faction state `trend` is always 0.
- Enum values seen outside the schema / reference code: station state `DamagedHuman` (3,104 stations),
  allegiance `Frontline Solutions`, economy `Rescue`/`Prison` in station economies. `parents` kinds here
  are only Null/Star/Planet (`Ring` not seen in this extract).
- Map keys are small closed sets in practice: body `timestamps` {meanAnomaly, distanceToArrival},
  system `timestamps` {factions, powerState, powers, controllingPower}, `solidComposition` {Ice, Metal, Rock},
  25 materials, 13 atmosphere gases, 20 body signal types, 16 ring signal types.
- Fleet carriers (`Drake-Class Carrier`) are 50k of the 327k space stations; Spansh-Ingester would drop all of them,
  plus 319k settlements.

## Surveys: galaxy_stations, galaxy_1month, galaxy_1day (2026-10-29)
Reports in `docs/survey/`. galaxy_1day was streamed over HTTP (no disk copy): 7.9 GB JSON in 84 s,
network-bound at ~14.5 MB/s compressed while two other downloads shared the line.

| dump | systems | JSON | time | stations | bodies |
|---|---|---|---|---|---|
| galaxy_stations | 143,635 | 30.0 GB | 62.8 s | 864,942 | 2.54M |
| galaxy_1month | 2,850,374 | 34.3 GB | 74.4 s | 736,240 | 10.6M |
| galaxy_1day (HTTP) | 97,609 | 7.9 GB | 83.1 s | 261,298 | 669k |

- Still 0 parse errors and 0 duplicate ids in every dump, including 56,009 fleet carriers in galaxy_stations:
  station `id` (market id) is safe as a primary key.
- `body id64 == system id64 | bodyId << 55` holds for all 16.6M bodies checked -> body PK = id64.
- `Ring` parent kind never appears (19.3M parent links in galaxy_1month alone): keep it as a lookup value, nothing special.
- Unpopulated systems simply omit `allegiance`, `government`, economies, `security`, `population`, `bodyCount`,
  `factions`, `controllingFaction`, `timestamps` (64% of galaxy_1month). `allegiance` is also present-but-null
  in 928k systems; null and missing will both map to SQL NULL (documented transformation).
- `population` can be 0. `thargoidWar` exists in exactly one system (war is over) - keep as nullable columns.
- More enum values outside the schema/reference lists: atmosphere `Thin Ammonia-rich`, `Hot Argon`,
  `Hot thick Nitrogen`; ~30 extra spectral classes; luminosity `IIa`, `IIb`; signal
  `$SAA_SignalType_PlanetAnomaly;`; ring signal `Bauxite`; system timestamp key `thargoidWar`.
  Confirms: enum-like strings go into lookup tables that grow at import time.
- Unexplored-system share does not slow the local pipeline: galaxy_1month ran at 461 MB/s (38k systems/s)
  vs 477 MB/s for populated - the cap again looks like single-threaded gzip.

## Source identity and resume (design)
Spansh (nginx) sends `ETag` (`"<mtime hex>-<size hex>"`), `Last-Modified`, `Content-Length`,
`Accept-Ranges: bytes`, and `Cache-Control: max-age=86400` (dumps are rebuilt daily, ~07:38 UTC).
A `Range: bytes=-8` request returns the gzip trailer: CRC32 + uncompressed size mod 2^32 of the
(last) gzip member - a content fingerprint that costs 8 bytes to fetch.

Each import run records a source fingerprint in the database, in the same table as its checkpoint:
- HTTP: URL, ETag, Last-Modified, Content-Length, gzip trailer (CRC32, ISIZE).
- Local file: canonical path, size, mtime, gzip trailer.

On resume the fingerprint is re-read and compared field by field. Any difference means a different file:
refuse to resume (or start a fresh run when explicitly asked), never mix data from two dump versions.
Every reconnect `Range` request also sends `If-Range: <etag>`, so a file replaced mid-download comes back
as `200` (full body) instead of `206`; treat that as "source changed" and abort rather than splice.
At EOF, flate2 has already verified the trailer CRC32 against the bytes it decoded.

## Open questions for the data survey
- Is `id64` unique across the dump? Is a station `id` (market id) unique, or do carriers appear twice?
- Range of `bodyId` (schema says u64; likely a small int) and of `age`, `population`, prices.
- Exact value sets of every enum-like field in real data, and which fields appear outside the schema.
- Null vs missing frequency per field, and the share of each nested structure (rings, signals, markets...).
