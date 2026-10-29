use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;

/// Opens a dump - local path or http(s) URL - as a buffered decompressed byte stream.
/// `.gz` is detected by extension; HTTP bodies are streamed, never stored.
pub fn open(source: &str) -> Result<Box<dyn BufRead + Send>> {
    // shortcut: a dropped connection fails the run; add Range-based reconnect before full-galaxy runs.
    let raw: Box<dyn Read + Send> = if is_http(source) {
        let resp = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()?
            .get(source)
            .send()
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("requesting {source}"))?;
        Box::new(resp)
    } else {
        Box::new(File::open(source).with_context(|| format!("opening {source}"))?)
    };
    let raw: Box<dyn Read + Send> = if source.ends_with(".gz") {
        Box::new(MultiGzDecoder::new(BufReader::with_capacity(1 << 20, raw)))
    } else {
        raw
    };
    Ok(Box::new(BufReader::with_capacity(1 << 20, raw)))
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
