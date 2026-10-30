# Configuration

Everything you can set, and how. For a first import on your own machine you need none of this: the
defaults connect to database `elite` on `localhost:5432` as user `postgres`.

## Connecting to the database

Use any of the usual PostgreSQL ways. Where they overlap, the earlier one in this list wins:

1. **A connection string**, in `--database` or the `DATABASE_URL` variable. Either form works:

   ```sh
   postgres://user:password@host:5432/elite?sslmode=require
   host=db.example user=me dbname=elite sslmode=require
   ```

2. **The standard PostgreSQL variables**:
   - `PGHOST`, `PGPORT`, `PGUSER` and `PGPASSWORD`;
   - `PGDATABASE`;
   - `PGSSLMODE` and `PGSSLROOTCERT`.

   They fill in whatever the connection string leaves out.

3. **A password file**:
   - Linux and macOS: `~/.pgpass`;
   - Windows: `%APPDATA%\postgresql\pgpass.conf`;
   - or the file named in `PGPASSFILE`.

   Each line is `host:port:database:user:password`, and `*` matches anything. This keeps the
   password out of commands and shell history.

**Special characters:**
- In the URL form, special characters in a password must be percent-encoded, for example `@` as `%40`.
- In the `key=value` form, a backslash escapes the next character, as in `psql`. Write Windows paths
  there with forward slashes (`sslrootcert='C:/certs/ca.pem'`) or doubled backslashes.

## Encryption

| `sslmode` | What it does |
|---|---|
| `prefer` *(default)* | Encrypts if the server supports it. |
| `require` | Always encrypts. Doesn't check the server's certificate, unless `sslrootcert` is given. |
| `verify-ca` | Encrypts and checks the certificate against `sslrootcert`, or the system's trusted certificates. |
| `verify-full` | Also checks that the certificate matches the host name. **Use this for cloud databases.** |
| `disable` | Never encrypts. |

Cloud providers publish the CA certificate to use as `sslrootcert`.

## Schema

All tables go into one schema, `spansh` by default. Choose another with `--schema` or `SPANSH_SCHEMA`.
Names are lower-case letters, digits and `_`.

A second schema lets you build a fresh import while the old one stays in use, then swap them:

```sql
ALTER SCHEMA spansh RENAME TO spansh_old;
ALTER SCHEMA spansh_new RENAME TO spansh;
DROP SCHEMA spansh_old CASCADE;
```

## Example with every setting

A remote server with encryption, the password in a password file, a custom schema and log folder,
and a local copy of the dump.

**Linux / macOS**

```sh
# Server, database, user, encryption and schema: set once per shell, or in your profile.
export PGHOST=db.example.com
export PGPORT=5432
export PGDATABASE=elite
export PGUSER=importer
export PGSSLMODE=verify-full
export PGSSLROOTCERT=$HOME/certs/db-ca.pem
export SPANSH_SCHEMA=galaxy

# Password file, readable only by you.
echo "db.example.com:5432:elite:importer:s3cret" >> ~/.pgpass
chmod 600 ~/.pgpass

spansh-importer init     --log-dir /var/log/spansh
spansh-importer ingest   --log-dir /var/log/spansh --source /data/dumps/galaxy.json.gz
spansh-importer finalize --log-dir /var/log/spansh
```

**Windows PowerShell**

```powershell
$env:PGHOST        = "db.example.com"
$env:PGPORT        = "5432"
$env:PGDATABASE    = "elite"
$env:PGUSER        = "importer"
$env:PGSSLMODE     = "verify-full"
$env:PGSSLROOTCERT = "C:\certs\db-ca.pem"
$env:SPANSH_SCHEMA = "galaxy"

New-Item -ItemType Directory -Force "$env:APPDATA\postgresql" | Out-Null
Add-Content "$env:APPDATA\postgresql\pgpass.conf" "db.example.com:5432:elite:importer:s3cret"

.\spansh-importer.exe init     --log-dir D:\logs\spansh
.\spansh-importer.exe ingest   --log-dir D:\logs\spansh --source D:\dumps\galaxy.json.gz
.\spansh-importer.exe finalize --log-dir D:\logs\spansh
```

**The same as one command**, with options instead of variables:

```sh
spansh-importer ingest \
  --database "postgres://importer:s3cret@db.example.com:5432/elite?sslmode=verify-full&sslrootcert=/home/me/certs/db-ca.pem" \
  --schema galaxy \
  --log-dir /var/log/spansh \
  --source https://downloads.spansh.co.uk/galaxy.json.gz
```

- **Option placement:** `--database` and `--source` go after the command name. `--schema` and
  `--log-dir` can go anywhere.
- **Same target everywhere:** give `init`, `ingest` and `finalize` the same database and schema.
  Environment variables make that automatic.
- **Passwords:** a password inside `--database` ends up in your shell history. The password file or
  `PGPASSWORD` avoids that.

## All commands and options

| Command | What it does |
|---|---|
| `init` | Creates empty tables. **Deletes any previous import in that schema.** |
| `ingest --source <url or file>` | Imports a dump. Run it again to resume. |
| `finalize` | Checks the data and builds indexes. Run it again to resume. |
| `survey <url or file> --out report.txt` | Profiles a dump without importing it. No database needed. |

| Option | Default | Used by | Meaning |
|---|---|---|---|
| `--database` | `$DATABASE_URL`, then `PG*` variables | all but `survey` | Which database to use |
| `--schema` | `$SPANSH_SCHEMA`, else `spansh` | all but `survey` | Schema for the tables |
| `--log-dir` | `logs` | all | Where log files go |
| `--source` | | `ingest` | Dump URL or file path (`.json` or `.json.gz`) |
| `--batch-mib` | `64` | `ingest` | Data saved per batch. An interruption loses at most one batch. |
| `--max-systems` | | `ingest` | Stop after this many systems, e.g. for a trial run. Resumable. |
| `--issue-cap` | `100000` | `ingest` | Data problems stored in `import_issue`. All of them are still counted. |
| `--no-spatial` | | `finalize` | Skip the 3D index for distance searches |
| `--no-flags` | | `finalize` | Skip the `system_flags` route-planning table |
| `--out`, `--workers` | `survey.txt`, `8` | `survey` | Report file and number of threads |

## Faster imports

A stock PostgreSQL install works, but bulk-load settings make a big import much faster.
[db/bulkload.conf](../db/bulkload.conf) is the configuration used for the timings in the README.
Include it from your `postgresql.conf` and adjust the memory values to your machine.
