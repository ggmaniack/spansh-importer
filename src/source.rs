use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;

/// Opens a dump - local path or http(s) URL - as a buffered decompressed byte stream.
/// `.gz` is detected by extension; HTTP bodies are streamed, never stored.
pub fn open(source: &str) -> Result<Box<dyn BufRead + Send>> {
    // shortcut: a dropped connection fails the run; add Range-based reconnect before full-galaxy runs.
    let raw: Box<dyn Read + Send> = if source.starts_with("http://") || source.starts_with("https://") {
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
