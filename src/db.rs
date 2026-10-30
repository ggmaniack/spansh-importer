//! Database connection settings, resolved the way libpq users expect:
//! 1. the connection string (`--database` / `DATABASE_URL`), as a URL or `key=value` pairs;
//! 2. the standard environment variables (`PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`,
//!    `PGSSLMODE`, `PGSSLROOTCERT`) for anything the string leaves out;
//! 3. the password file (`PGPASSFILE`, else `~/.pgpass`, or `%APPDATA%\postgresql\pgpass.conf`);
//! 4. defaults: `localhost:5432`, user `postgres`, database `elite`, `sslmode=prefer`.
//!
//! `sslmode` follows libpq: `disable`, `allow`/`prefer` (TLS when the server offers it), `require`
//! (encrypted, certificate not checked unless `sslrootcert` is given), `verify-ca`, `verify-full`.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use postgres::config::{Host, SslMode};
use postgres::{Client, Config, NoTls};

/// Connects with `search_path` set to the import schema (then `public`, for extensions), so every
/// statement uses unqualified table names. `schema` is a validated plain identifier.
pub fn connect(conninfo: &str, schema: &str) -> Result<Client> {
    // An empty variable counts as unset.
    let s = Settings::resolve(conninfo, &|k| std::env::var(k).ok().filter(|v| !v.is_empty()), &pgpass_file())?;
    let mut c = s.connect().with_context(|| format!("connecting to {}", s.describe()))?;
    c.batch_execute(&format!("SET search_path = {schema}, public"))?;
    Ok(c)
}

struct Settings {
    config: Config,
    sslmode: String,
    rootcert: Option<String>,
}

impl Settings {
    fn resolve(conninfo: &str, env: &dyn Fn(&str) -> Option<String>, pgpass: &Option<PathBuf>) -> Result<Self> {
        let (rest, mut sslmode, mut rootcert) = take_tls_params(conninfo)?;
        let mut config = if rest.trim().is_empty() { Config::new() } else { Config::from_str(&rest).context("invalid connection string")? };
        if config.get_hosts().is_empty() {
            config.host(&env("PGHOST").unwrap_or_else(|| "localhost".into()));
        }
        if config.get_ports().is_empty() {
            if let Some(p) = env("PGPORT") {
                config.port(p.parse().with_context(|| format!("invalid PGPORT {p:?}"))?);
            }
        }
        if config.get_user().is_none() {
            config.user(&env("PGUSER").unwrap_or_else(|| "postgres".into()));
        }
        if config.get_dbname().is_none() {
            config.dbname(&env("PGDATABASE").unwrap_or_else(|| "elite".into()));
        }
        if config.get_password().is_none() {
            let from_file = || {
                let path = pgpass.as_ref()?;
                let text = std::fs::read_to_string(path).ok()?;
                let host = match config.get_hosts().first()? {
                    Host::Tcp(h) => h.clone(),
                    #[cfg(unix)]
                    Host::Unix(_) => "localhost".into(),
                };
                let port = config.get_ports().first().copied().unwrap_or(5432).to_string();
                pgpass_lookup(&text, &host, &port, config.get_dbname()?, config.get_user()?)
            };
            if let Some(p) = env("PGPASSWORD").or_else(from_file) {
                config.password(p);
            }
        }
        sslmode = sslmode.or_else(|| env("PGSSLMODE"));
        rootcert = rootcert.or_else(|| env("PGSSLROOTCERT"));
        let sslmode = sslmode.unwrap_or_else(|| "prefer".into());
        config.ssl_mode(match sslmode.as_str() {
            "disable" => SslMode::Disable,
            "allow" | "prefer" => SslMode::Prefer,
            "require" | "verify-ca" | "verify-full" => SslMode::Require,
            other => bail!("unknown sslmode {other:?} (use disable, prefer, require, verify-ca or verify-full)"),
        });
        Ok(Settings { config, sslmode, rootcert })
    }

    fn connect(&self) -> Result<Client> {
        if self.sslmode == "disable" {
            return Ok(self.config.connect(NoTls)?);
        }
        let mut b = native_tls::TlsConnector::builder();
        let custom_root = self.rootcert.as_deref().filter(|r| *r != "system");
        // libpq: with a root certificate file, `require` verifies the chain like `verify-ca`.
        let verify_chain = matches!(self.sslmode.as_str(), "verify-ca" | "verify-full") || (self.sslmode == "require" && custom_root.is_some());
        b.danger_accept_invalid_certs(!verify_chain);
        b.danger_accept_invalid_hostnames(self.sslmode != "verify-full");
        if let Some(path) = custom_root {
            let pem = std::fs::read(path).with_context(|| format!("reading sslrootcert {path}"))?;
            for cert in pem_certificates(&pem) {
                b.add_root_certificate(native_tls::Certificate::from_pem(&cert).with_context(|| format!("parsing {path}"))?);
            }
            b.disable_built_in_roots(true);
        }
        let tls = postgres_native_tls::MakeTlsConnector::new(b.build()?);
        Ok(self.config.connect(tls)?)
    }

    /// `user@host:port/db (sslmode)`, never the password.
    fn describe(&self) -> String {
        let host = match self.config.get_hosts().first() {
            Some(Host::Tcp(h)) => h.clone(),
            #[cfg(unix)]
            Some(Host::Unix(p)) => p.display().to_string(),
            None => String::new(),
        };
        let port = self.config.get_ports().first().copied().unwrap_or(5432);
        format!("{}@{host}:{port}/{} (sslmode={})", self.config.get_user().unwrap_or(""), self.config.get_dbname().unwrap_or(""), self.sslmode)
    }
}

/// Removes `sslmode` and `sslrootcert` from a URL or `key=value` connection string (the driver
/// only knows `disable`/`prefer`/`require`), returning them separately.
fn take_tls_params(conninfo: &str) -> Result<(String, Option<String>, Option<String>)> {
    let (mut mode, mut root) = (None, None);
    let mut take = |k: &str, v: String| match k {
        "sslmode" => {
            mode = Some(v);
            true
        }
        "sslrootcert" => {
            root = Some(v);
            true
        }
        _ => false,
    };
    if conninfo.starts_with("postgres://") || conninfo.starts_with("postgresql://") {
        let Some((base, query)) = conninfo.split_once('?') else { return Ok((conninfo.to_owned(), None, None)) };
        let kept: Vec<&str> = query
            .split('&')
            .filter(|p| match p.split_once('=') {
                Some((k, v)) => !take(k, percent_decode(v)),
                None => true,
            })
            .collect();
        let rest = if kept.is_empty() { base.to_owned() } else { format!("{base}?{}", kept.join("&")) };
        return Ok((rest, mode, root));
    }
    let mut kept = Vec::new();
    for (k, v) in key_values(conninfo)? {
        if !take(&k, v.clone()) {
            kept.push(format!("{k}='{}'", v.replace('\\', "\\\\").replace('\'', "\\'")));
        }
    }
    Ok((kept.join(" "), mode, root))
}

/// Parses libpq `key = value` pairs; values may be single-quoted with `\'` and `\\` escapes.
fn key_values(s: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    loop {
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if it.peek().is_none() {
            return Ok(out);
        }
        let key: String = std::iter::from_fn(|| it.next_if(|c| !c.is_whitespace() && *c != '=')).collect();
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if it.next() != Some('=') {
            bail!("invalid connection string: expected '=' after {key:?}");
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if it.next_if_eq(&'\'').is_some() {
            loop {
                match it.next() {
                    Some('\\') => value.extend(it.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => bail!("invalid connection string: unterminated quote in {key:?}"),
                }
            }
        } else {
            while let Some(c) = it.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    value.extend(it.next());
                } else {
                    value.push(c);
                }
            }
        }
        out.push((key, value));
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pem_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    const END: &str = "-----END CERTIFICATE-----";
    String::from_utf8_lossy(pem).split_inclusive(END).filter(|c| c.contains(END)).map(|c| c.trim().as_bytes().to_vec()).collect()
}

fn pgpass_file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PGPASSFILE").filter(|p| !p.is_empty()) {
        return Some(p.into());
    }
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("postgresql").join("pgpass.conf"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pgpass"))
    }
}

/// First `host:port:database:user:password` line matching the target; `*` matches anything and
/// `\:` / `\\` escape within fields.
fn pgpass_lookup(text: &str, host: &str, port: &str, db: &str, user: &str) -> Option<String> {
    text.lines().filter(|l| !l.trim_start().starts_with('#')).find_map(|line| {
        let mut fields = vec![String::new()];
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => fields.last_mut()?.extend(chars.next()),
                ':' if fields.len() < 5 => fields.push(String::new()),
                c => fields.last_mut()?.push(c),
            }
        }
        let [h, p, d, u, pw] = <[String; 5]>::try_from(fields).ok()?;
        let ok = |f: &str, v: &str| f == "*" || f == v;
        (ok(&h, host) && ok(&p, port) && ok(&d, db) && ok(&u, user)).then_some(pw)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn takes_tls_params_out_of_both_string_forms() {
        let (rest, mode, root) = take_tls_params("postgres://u:p@h:6000/db?sslmode=verify-full&application_name=x&sslrootcert=C%3A%5Cca.pem").unwrap();
        assert_eq!(rest, "postgres://u:p@h:6000/db?application_name=x");
        assert_eq!(mode.as_deref(), Some("verify-full"));
        assert_eq!(root.as_deref(), Some(r"C:\ca.pem"));
        let (rest, mode, root) = take_tls_params(r"host=h password='it\'s a \\secret' sslmode = require dbname=elite").unwrap();
        assert_eq!(rest, r"host='h' password='it\'s a \\secret' dbname='elite'");
        assert_eq!((mode.as_deref(), root), (Some("require"), None));
        assert!(take_tls_params("host='unterminated").is_err());
    }

    #[test]
    fn layers_string_then_environment_then_defaults() {
        let s = Settings::resolve("", &env(&[]), &None).unwrap();
        assert_eq!(s.describe(), "postgres@localhost:5432/elite (sslmode=prefer)");
        let vars = env(&[("PGHOST", "db.example"), ("PGPORT", "6543"), ("PGUSER", "cmdr"), ("PGDATABASE", "galaxy"), ("PGPASSWORD", "pw"), ("PGSSLMODE", "verify-full")]);
        let s = Settings::resolve("", &vars, &None).unwrap();
        assert_eq!(s.describe(), "cmdr@db.example:6543/galaxy (sslmode=verify-full)");
        assert_eq!(s.config.get_password(), Some(&b"pw"[..]));
        // The connection string wins over the environment.
        let s = Settings::resolve("postgres://other@h2/db2?sslmode=disable", &vars, &None).unwrap();
        assert_eq!(s.describe(), "other@h2:5432/db2 (sslmode=disable)");
        assert!(Settings::resolve("sslmode=bogus", &env(&[]), &None).is_err());
    }

    #[test]
    fn reads_the_password_file() {
        let text = "# comment\nother:*:*:*:nope\ndb.example:6543:*:cmdr:s\\:e\\\\cret\n*:*:*:*:fallback\n";
        assert_eq!(pgpass_lookup(text, "db.example", "6543", "galaxy", "cmdr").as_deref(), Some(r"s:e\cret"));
        assert_eq!(pgpass_lookup(text, "localhost", "5432", "elite", "postgres").as_deref(), Some("fallback"));
        let dir = std::env::temp_dir().join(format!("pgpass-test-{}", std::process::id()));
        std::fs::write(&dir, text).unwrap();
        let s = Settings::resolve("postgres://cmdr@db.example:6543/galaxy", &env(&[]), &Some(dir.clone())).unwrap();
        assert_eq!(s.config.get_password(), Some(&b"s:e\\cret"[..]));
        let _ = std::fs::remove_file(dir);
    }
}
