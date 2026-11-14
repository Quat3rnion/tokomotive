//! Bounded AWS event-stream framing with IEEE CRC32 validation.
//! https://docs.aws.amazon.com/transcribe/latest/dg/streaming-setting-up.html#streaming-event-stream
use std::collections::BTreeMap;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MAX_HEADERS: usize = 64;
#[derive(Debug)]
pub struct Frame {
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
}
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    failed: bool,
}
fn u32_at(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("checked size"))
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, &'static str> {
        if self.failed {
            return Err("event-stream decoder failed");
        }
        let result = self.decode(bytes);
        if result.is_err() {
            self.failed = true;
            self.buf.clear();
        }
        result
    }
    fn decode(&mut self, mut bytes: &[u8]) -> Result<Vec<Frame>, &'static str> {
        let mut frames = Vec::new();
        while !bytes.is_empty() {
            let target = if self.buf.len() < 12 {
                12
            } else {
                u32_at(&self.buf) as usize
            };
            let take = (target - self.buf.len()).min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() < 12 {
                continue;
            }
            let total = u32_at(&self.buf) as usize;
            let hlen = u32_at(&self.buf[4..]) as usize;
            if !(16..=MAX_FRAME).contains(&total) || hlen > total - 16 || hlen > 128 * 1024 {
                return Err("invalid event-stream frame size");
            }
            if crc32fast::hash(&self.buf[..8]) != u32_at(&self.buf[8..]) {
                return Err("event-stream prelude CRC mismatch");
            }
            if self.buf.len() < total {
                continue;
            }
            if crc32fast::hash(&self.buf[..total - 4]) != u32_at(&self.buf[total - 4..]) {
                return Err("event-stream message CRC mismatch");
            }
            let mut headers = BTreeMap::new();
            let mut h = &self.buf[12..12 + hlen];
            let mut count = 0;
            while !h.is_empty() {
                count += 1;
                if count > MAX_HEADERS {
                    return Err("too many event-stream headers");
                }
                let n = usize::from(h[0]);
                if n == 0 || h.len() < n + 2 {
                    return Err("truncated event-stream header");
                }
                let name = std::str::from_utf8(&h[1..1 + n])
                    .map_err(|_| "invalid header name")?
                    .to_owned();
                let kind = h[1 + n];
                h = &h[n + 2..];
                let (len, prefix) = match kind {
                    0 | 1 => (0, 0),
                    2 => (1, 0),
                    3 => (2, 0),
                    4 => (4, 0),
                    5 | 8 => (8, 0),
                    9 => (16, 0),
                    6 | 7 => {
                        if h.len() < 2 {
                            return Err("truncated event-stream header length");
                        }
                        (usize::from(u16::from_be_bytes([h[0], h[1]])), 2)
                    }
                    _ => return Err("invalid event-stream header type"),
                };
                if h.len() < prefix + len {
                    return Err("truncated event-stream header value");
                }
                if kind == 7 {
                    let value = std::str::from_utf8(&h[prefix..prefix + len])
                        .map_err(|_| "invalid header value")?
                        .to_owned();
                    if headers.insert(name, value).is_some() {
                        return Err("duplicate event-stream header");
                    }
                }
                h = &h[prefix + len..];
            }
            frames.push(Frame {
                headers,
                payload: self.buf[12 + hlen..total - 4].to_vec(),
            });
            self.buf.clear();
        }
        Ok(frames)
    }
    pub fn finish(&self) -> Result<(), &'static str> {
        if self.failed || !self.buf.is_empty() {
            Err("truncated or invalid event-stream")
        } else {
            Ok(())
        }
    }
}
