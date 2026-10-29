//! Encoder for PostgreSQL's binary COPY format. Rows are encoded straight into a byte buffer,
//! so a batch's memory footprint is exactly the bytes that will be sent.

pub const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";
pub const TRAILER: &[u8] = &(-1i16).to_be_bytes();

const OID_INT2: u32 = 21;
const OID_INT4: u32 = 23;
const OID_TEXT: u32 = 25;

/// Microseconds between 1970-01-01 and PostgreSQL's epoch 2000-01-01.
const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000;

#[derive(Default)]
pub struct CopyBuf {
    pub buf: Vec<u8>,
    pub rows: u64,
}

impl CopyBuf {
    pub fn row(&mut self, fields: i16) -> &mut Self {
        self.rows += 1;
        self.buf.extend_from_slice(&fields.to_be_bytes());
        self
    }

    fn null(&mut self) -> &mut Self {
        self.buf.extend_from_slice(&(-1i32).to_be_bytes());
        self
    }

    fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
        self.buf.extend_from_slice(bytes);
        self
    }

    pub fn i16(&mut self, v: Option<i16>) -> &mut Self {
        match v {
            Some(v) => self.raw(&v.to_be_bytes()),
            None => self.null(),
        }
    }

    pub fn i32(&mut self, v: Option<i32>) -> &mut Self {
        match v {
            Some(v) => self.raw(&v.to_be_bytes()),
            None => self.null(),
        }
    }

    pub fn i64(&mut self, v: Option<i64>) -> &mut Self {
        match v {
            Some(v) => self.raw(&v.to_be_bytes()),
            None => self.null(),
        }
    }

    pub fn f64(&mut self, v: Option<f64>) -> &mut Self {
        match v {
            Some(v) => self.raw(&v.to_bits().to_be_bytes()),
            None => self.null(),
        }
    }

    pub fn bool(&mut self, v: Option<bool>) -> &mut Self {
        match v {
            Some(v) => self.raw(&[v as u8]),
            None => self.null(),
        }
    }

    pub fn text(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            Some(v) => self.raw(v.as_bytes()),
            None => self.null(),
        }
    }

    /// TIMESTAMPTZ from microseconds since the Unix epoch (UTC).
    pub fn ts(&mut self, unix_us: Option<i64>) -> &mut Self {
        self.i64(unix_us.map(|us| us - PG_EPOCH_OFFSET_US))
    }

    /// JSONB: version byte 1 followed by the JSON text.
    pub fn jsonb(&mut self, v: Option<&serde_json::Map<String, serde_json::Value>>) -> &mut Self {
        match v {
            Some(m) => {
                let json = serde_json::to_vec(m).expect("serializing a JSON map cannot fail");
                self.buf.extend_from_slice(&(json.len() as i32 + 1).to_be_bytes());
                self.buf.push(1);
                self.buf.extend_from_slice(&json);
                self
            }
            None => self.null(),
        }
    }

    fn array<T>(&mut self, oid: u32, items: &[T], elem_len: usize, mut put: impl FnMut(&mut Vec<u8>, &T)) -> &mut Self {
        let dims = if items.is_empty() { 0 } else { 1 };
        let len = 12 + dims * 8 + items.len() * 4 + items.len() * elem_len;
        let start = self.buf.len();
        self.buf.extend_from_slice(&(len as i32).to_be_bytes());
        self.buf.extend_from_slice(&(dims as i32).to_be_bytes());
        self.buf.extend_from_slice(&0i32.to_be_bytes()); // no NULL elements
        self.buf.extend_from_slice(&oid.to_be_bytes());
        if dims == 1 {
            self.buf.extend_from_slice(&(items.len() as i32).to_be_bytes());
            self.buf.extend_from_slice(&1i32.to_be_bytes()); // lower bound
        }
        for it in items {
            put(&mut self.buf, it);
        }
        // Variable-length elements (text) cannot precompute the length; patch it.
        let actual = (self.buf.len() - start - 4) as i32;
        self.buf[start..start + 4].copy_from_slice(&actual.to_be_bytes());
        self
    }

    pub fn i16_array(&mut self, v: Option<&[i16]>) -> &mut Self {
        match v {
            Some(a) => self.array(OID_INT2, a, 2, |b, x| {
                b.extend_from_slice(&2i32.to_be_bytes());
                b.extend_from_slice(&x.to_be_bytes());
            }),
            None => self.null(),
        }
    }

    pub fn i32_array(&mut self, v: Option<&[i32]>) -> &mut Self {
        match v {
            Some(a) => self.array(OID_INT4, a, 4, |b, x| {
                b.extend_from_slice(&4i32.to_be_bytes());
                b.extend_from_slice(&x.to_be_bytes());
            }),
            None => self.null(),
        }
    }

    pub fn text_array(&mut self, v: Option<&[String]>) -> &mut Self {
        match v {
            Some(a) => self.array(OID_TEXT, a, 0, |b, x| {
                b.extend_from_slice(&(x.len() as i32).to_be_bytes());
                b.extend_from_slice(x.as_bytes());
            }),
            None => self.null(),
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.rows = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_scalars_and_nulls() {
        let mut c = CopyBuf::default();
        c.row(3).i16(Some(-2)).f64(None).text(Some("ab"));
        assert_eq!(c.rows, 1);
        assert_eq!(c.buf, [0, 3, 0, 0, 0, 2, 0xff, 0xfe, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 2, b'a', b'b']);
    }

    #[test]
    fn encodes_arrays() {
        let mut c = CopyBuf::default();
        c.i16_array(Some(&[7]));
        // len=26: ndim 1, flags 0, oid 21, dim 1, lbound 1, elem (len 2, value 7)
        assert_eq!(&c.buf[..4], &26i32.to_be_bytes());
        assert_eq!(&c.buf[4..8], &1i32.to_be_bytes());
        assert_eq!(&c.buf[12..16], &21u32.to_be_bytes());
        assert_eq!(&c.buf[c.buf.len() - 2..], &7i16.to_be_bytes());

        let mut e = CopyBuf::default();
        e.text_array(Some(&[]));
        assert_eq!(e.buf, [0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 25]);

        let mut t = CopyBuf::default();
        t.text_array(Some(&["xy".to_string()]));
        assert_eq!(&t.buf[..4], &(12 + 8 + 4 + 2i32).to_be_bytes());
    }

    #[test]
    fn timestamps_use_postgres_epoch() {
        let mut c = CopyBuf::default();
        c.ts(Some(946_684_800_000_000));
        assert_eq!(&c.buf[4..], &0i64.to_be_bytes());
    }
}
