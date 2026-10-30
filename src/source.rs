use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;

use crate::report::Level;

/// An opened dump: the decompressed stream, plus how many bytes of the source file (compressed, if
/// `.gz`) have been read so far out of `size`.
pub struct Input {
    pub reader: Box<dyn BufRead + Send>,
    pub read: Arc<AtomicU64>,
    pub size: Option<u64>,
}

struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Opens a dump - local path or http(s) URL - as a buffered decompressed byte stream.
/// `.gz` is detected by extension; HTTP bodies are streamed, never stored.
pub fn open(source: &str) -> Result<Input> {
    let (raw, size): (Box<dyn Read + Send>, Option<u64>) = if is_http(source) {
        let r = HttpReader::open(source)?;
        let size = r.resp.content_length();
        (Box::new(r), size)
    } else {
        let f = File::open(source).with_context(|| format!("opening {source}"))?;
        let size = f.metadata().ok().map(|m| m.len());
        (Box::new(f), size)
    };
    let read = Arc::new(AtomicU64::new(0));
    let raw: Box<dyn Read + Send> = Box::new(Counting { inner: raw, read: read.clone() });
    let raw: Box<dyn Read + Send> = if source.ends_with(".gz") {
        Box::new(MultiGzDecoder::new(BufReader::with_capacity(1 << 20, raw)))
    } else {
        raw
    };
    Ok(Input { reader: Box::new(BufReader::with_capacity(1 << 20, raw)), read, size })
}

/// HTTP body reader that survives dropped connections: it re-requests the remaining bytes with
/// `Range` + `If-Range: <validator>`, so a file replaced on the server comes back as 200 (full
/// body) and is rejected instead of being spliced onto the old one.
struct HttpReader {
    client: reqwest::blocking::Client,
    url: String,
    validator: String,
    offset: u64,
    resp: reqwest::blocking::Response,
}

const MAX_CONSECUTIVE_FAILURES: u32 = 20;

impl HttpReader {
    fn open(url: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            // No total timeout (downloads take hours); keepalive turns a dead peer into a read error.
            .timeout(None)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client.get(url).send().and_then(|r| r.error_for_status()).with_context(|| format!("requesting {url}"))?;
        let header = |n| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let validator = header("etag").or_else(|| header("last-modified")).context("server sends neither ETag nor Last-Modified; cannot resume safely")?;
        Ok(HttpReader { client, url: url.to_owned(), validator, offset: 0, resp })
    }

    fn reconnect(&mut self) -> std::io::Result<()> {
        let resp = self
            .client
            .get(&self.url)
            .header("Range", format!("bytes={}-", self.offset))
            .header("If-Range", &self.validator)
            .send()
            .map_err(std::io::Error::other)?;
        match resp.status().as_u16() {
            206 => {
                self.resp = resp;
                Ok(())
            }
            200 => Err(std::io::Error::other(format!("{} changed on the server during the download (If-Range mismatch)", self.url))),
            s => Err(std::io::Error::other(format!("unexpected HTTP {s} when resuming {}", self.url))),
        }
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut failures = 0;
        loop {
            match self.resp.read(buf) {
                Ok(n) => {
                    self.offset += n as u64;
                    return Ok(n);
                }
                Err(e) if failures < MAX_CONSECUTIVE_FAILURES => {
                    failures += 1;
                    let wait = std::time::Duration::from_secs((1u64 << failures.min(6)).min(60));
                    crate::report::log(Level::Warn, "reconnect", &format!("connection lost at byte {} ({e}); reconnect {failures} in {wait:?}", self.offset));
                    std::thread::sleep(wait);
                    if let Err(e) = self.reconnect() {
                        if e.to_string().contains("changed on the server") {
                            return Err(e);
                        }
                        crate::report::log(Level::Warn, "reconnect_failed", &format!("reconnect failed: {e}"));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Identity of a source file; a resume is only allowed when every field matches the stored run.
#[derive(Debug, Default)]
pub struct Fingerprint {
    pub source: String,
    pub content_length: Option<i64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub file_mtime: Option<std::time::SystemTime>,
    /// gzip trailer of the last member: CRC32 and uncompressed size mod 2^32.
    pub gzip_crc32: Option<i64>,
    pub gzip_isize: Option<i64>,
}

fn is_http(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

pub fn fingerprint(source: &str) -> Result<Fingerprint> {
    let gz = source.ends_with(".gz");
    let trailer = |b: &[u8]| -> (Option<i64>, Option<i64>) {
        match b {
            [c0, c1, c2, c3, s0, s1, s2, s3] if gz => (
                Some(u32::from_le_bytes([*c0, *c1, *c2, *c3]) as i64),
                Some(u32::from_le_bytes([*s0, *s1, *s2, *s3]) as i64),
            ),
            _ => (None, None),
        }
    };
    if is_http(source) {
        let client = reqwest::blocking::Client::new();
        let head = client.head(source).send().and_then(|r| r.error_for_status()).with_context(|| format!("HEAD {source}"))?;
        let header = |name: &str| head.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let tail = if gz {
            client.get(source).header("Range", "bytes=-8").send().and_then(|r| r.error_for_status())?.bytes()?.to_vec()
        } else {
            Vec::new()
        };
        let (gzip_crc32, gzip_isize) = trailer(&tail);
        Ok(Fingerprint {
            source: source.to_owned(),
            content_length: header("content-length").and_then(|v| v.parse().ok()),
            etag: header("etag"),
            last_modified: header("last-modified"),
            file_mtime: None,
            gzip_crc32,
            gzip_isize,
        })
    } else {
        use std::io::{Seek, SeekFrom};
        let path = std::fs::canonicalize(source).with_context(|| format!("resolving {source}"))?;
        let mut file = File::open(&path)?;
        let meta = file.metadata()?;
        let mut tail = Vec::new();
        if gz && meta.len() >= 8 {
            file.seek(SeekFrom::End(-8))?;
            file.read_to_end(&mut tail)?;
        }
        let (gzip_crc32, gzip_isize) = trailer(&tail);
        Ok(Fingerprint {
            source: path.display().to_string(),
            content_length: Some(meta.len() as i64),
            etag: None,
            last_modified: None,
            file_mtime: meta.modified().ok(),
            gzip_crc32,
            gzip_isize,
        })
    }
}

/// Strips one dump line down to the system object: real dumps hold `[`, then one
/// `{...},` per line, then `]`. Returns `None` for the bracket and blank lines.
pub fn system_bytes(line: &[u8]) -> Option<&[u8]> {
    let mut s = line.trim_ascii();
    if let Some(rest) = s.strip_suffix(b",") {
        s = rest.trim_ascii_end();
    }
    match s {
        b"" | b"[" | b"]" => None,
        _ => Some(s),
    }
}

#[cfg(test)]
mod tests {
    use super::system_bytes;

    #[test]
    fn strips_dump_line_framing() {
        assert_eq!(system_bytes(b"[\n"), None);
        assert_eq!(system_bytes(b"]"), None);
        assert_eq!(system_bytes(b"   \r\n"), None);
        assert_eq!(system_bytes(b"    {\"a\":1},\n"), Some(&b"{\"a\":1}"[..]));
        assert_eq!(system_bytes(b"    {\"a\":\"x,\"}\r\n"), Some(&b"{\"a\":\"x,\"}"[..]));
    }
}
