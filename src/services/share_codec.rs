//! Binary framing for the `aivo-tunnel/2` wire protocol — CLI side.
//!
//! The wire format is canonical in `s.getaivo.dev/protocol.md`. This module
//! mirrors the layout from the CLI's perspective: we encode the frames we
//! send (`ClientFrame`) and decode the frames we receive (`ServerFrame`).
//!
//! One frame per WebSocket binary message. All multi-byte integers are
//! big-endian. Strings are UTF-8; header names are lowercase ASCII.
//!
//! v2 replaced v1's JSON-over-text + base64 framing. Response bodies are
//! shipped as raw bytes after a 6-byte header (1 type + 4 id + 1 flags) —
//! no envelope, no expansion.
//!
//! Keep this in lockstep with `s.getaivo.dev/src/codec.rs`. If you change
//! a tag value or field width, change the spec first.

use anyhow::{Context, Result, bail};

pub const SUBPROTOCOL: &str = "aivo-tunnel/2";

pub const FRAME_REGISTER: u8 = 0x01;
pub const FRAME_REGISTERED: u8 = 0x02;
pub const FRAME_REQUEST: u8 = 0x10;
pub const FRAME_RESPONSE_HEAD: u8 = 0x20;
pub const FRAME_RESPONSE_CHUNK: u8 = 0x21;
pub const FRAME_PING: u8 = 0xF0;
pub const FRAME_PONG: u8 = 0xF1;
pub const FRAME_REJECT: u8 = 0xFF;

pub const METHOD_GET: u8 = 0;
pub const METHOD_HEAD: u8 = 1;

pub const FLAG_LAST: u8 = 0x01;

/// Frames the CLI receives from `s.getaivo.dev`.
#[derive(Debug)]
pub enum ServerFrame {
    Registered {
        slot_id: String,
        url: String,
        max_bytes_per_request: u32,
        ping_interval_ms: u16,
    },
    Request {
        id: u32,
        method: u8,
        path: String,
        #[allow(dead_code)]
        headers: Vec<(String, String)>,
        #[allow(dead_code)]
        body: Vec<u8>,
    },
    Ping,
    Reject {
        reason: String,
        #[allow(dead_code)]
        retry_after_secs: u16,
    },
}

impl ServerFrame {
    pub fn decode(buf: &[u8]) -> Result<ServerFrame> {
        let mut r = Reader::new(buf);
        let ty = r.u8().context("frame type")?;
        match ty {
            FRAME_REGISTERED => {
                let slot_id = r.str_u8().context("slot_id")?.to_string();
                let url = r.str_u16().context("url")?.to_string();
                let max_bytes_per_request = r.u32().context("max_bytes")?;
                let ping_interval_ms = r.u16().context("ping_interval")?;
                Ok(ServerFrame::Registered {
                    slot_id,
                    url,
                    max_bytes_per_request,
                    ping_interval_ms,
                })
            }
            FRAME_REQUEST => {
                let id = r.u32().context("id")?;
                let method = r.u8().context("method")?;
                let path = r.str_u16().context("path")?.to_string();
                let headers = r.headers().context("headers")?;
                let body_len = r.u32().context("body_len")? as usize;
                let body = r.take(body_len).context("body")?.to_vec();
                Ok(ServerFrame::Request {
                    id,
                    method,
                    path,
                    headers,
                    body,
                })
            }
            FRAME_PING => Ok(ServerFrame::Ping),
            FRAME_REJECT => {
                let reason = r.str_u8().context("reason")?.to_string();
                let retry_after_secs = r.u16().context("retry_after")?;
                Ok(ServerFrame::Reject {
                    reason,
                    retry_after_secs,
                })
            }
            t => bail!("unknown server frame type 0x{t:02x}"),
        }
    }
}

/// Frames the CLI sends. Encoded by value because the hot path (response
/// chunks) carries owned body bytes from the local HTTP client.
pub enum ClientFrame<'a> {
    Register {
        info_json: &'a [u8],
    },
    ResponseHead {
        id: u32,
        status: u16,
        headers: &'a [(String, String)],
    },
    ResponseChunk {
        id: u32,
        last: bool,
        body: &'a [u8],
    },
    Pong,
}

impl ClientFrame<'_> {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            ClientFrame::Register { info_json } => {
                let mut out = Vec::with_capacity(1 + 2 + info_json.len());
                out.push(FRAME_REGISTER);
                put_u16_len(&mut out, info_json.len());
                out.extend_from_slice(info_json);
                out
            }
            ClientFrame::ResponseHead {
                id,
                status,
                headers,
            } => {
                let hdr_bytes: usize = headers.iter().map(|(k, v)| 1 + k.len() + 2 + v.len()).sum();
                let mut out = Vec::with_capacity(1 + 4 + 2 + 1 + hdr_bytes);
                out.push(FRAME_RESPONSE_HEAD);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&status.to_be_bytes());
                put_headers(&mut out, headers);
                out
            }
            ClientFrame::ResponseChunk { id, last, body } => {
                let mut out = Vec::with_capacity(1 + 4 + 1 + body.len());
                out.push(FRAME_RESPONSE_CHUNK);
                out.extend_from_slice(&id.to_be_bytes());
                out.push(if last { FLAG_LAST } else { 0 });
                out.extend_from_slice(body);
                out
            }
            ClientFrame::Pong => vec![FRAME_PONG],
        }
    }
}

// --- low-level helpers ----------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("truncated frame (need {n}, have {})", self.buf.len());
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str_u8(&mut self) -> Result<&'a str> {
        let n = self.u8()? as usize;
        std::str::from_utf8(self.take(n)?).context("utf-8")
    }
    fn str_u16(&mut self) -> Result<&'a str> {
        let n = self.u16()? as usize;
        std::str::from_utf8(self.take(n)?).context("utf-8")
    }
    fn headers(&mut self) -> Result<Vec<(String, String)>> {
        let n = self.u8()? as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let k = self.str_u8()?.to_string();
            let val = self.str_u16()?.to_string();
            v.push((k, val));
        }
        Ok(v)
    }
}

fn put_u16_len(out: &mut Vec<u8>, n: usize) {
    let n = u16::try_from(n).expect("string > 65535 bytes for u16-prefixed field");
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_str_u8(out: &mut Vec<u8>, s: &str) {
    let n = u8::try_from(s.len()).expect("string > 255 bytes for u8-prefixed field");
    out.push(n);
    out.extend_from_slice(s.as_bytes());
}

fn put_str_u16(out: &mut Vec<u8>, s: &str) {
    put_u16_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn put_headers(out: &mut Vec<u8>, headers: &[(String, String)]) {
    let n = u8::try_from(headers.len()).expect("> 255 headers");
    out.push(n);
    for (k, v) in headers {
        put_str_u8(out, k);
        put_str_u16(out, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_decodes() {
        // Build a registered frame manually.
        let mut buf = vec![FRAME_REGISTERED];
        put_str_u8(&mut buf, "abc12345defg");
        put_str_u16(&mut buf, "https://s.getaivo.dev/s/abc12345defg");
        buf.extend_from_slice(&(32u32 * 1024 * 1024).to_be_bytes());
        buf.extend_from_slice(&20_000u16.to_be_bytes());
        match ServerFrame::decode(&buf).unwrap() {
            ServerFrame::Registered {
                slot_id,
                url,
                max_bytes_per_request,
                ping_interval_ms,
            } => {
                assert_eq!(slot_id, "abc12345defg");
                assert_eq!(url, "https://s.getaivo.dev/s/abc12345defg");
                assert_eq!(max_bytes_per_request, 32 * 1024 * 1024);
                assert_eq!(ping_interval_ms, 20_000);
            }
            f => panic!("wrong variant: {f:?}"),
        }
    }

    #[test]
    fn request_decodes_with_headers_and_body() {
        let mut buf = vec![FRAME_REQUEST];
        buf.extend_from_slice(&17u32.to_be_bytes());
        buf.push(METHOD_GET);
        put_str_u16(&mut buf, "/state?wait=30&since=3%3A7");
        // 1 header: accept = application/json
        buf.push(1);
        put_str_u8(&mut buf, "accept");
        put_str_u16(&mut buf, "application/json");
        // empty body
        buf.extend_from_slice(&0u32.to_be_bytes());
        match ServerFrame::decode(&buf).unwrap() {
            ServerFrame::Request {
                id,
                method,
                path,
                headers,
                body,
            } => {
                assert_eq!(id, 17);
                assert_eq!(method, METHOD_GET);
                assert_eq!(path, "/state?wait=30&since=3%3A7");
                assert_eq!(headers, vec![("accept".into(), "application/json".into())]);
                assert!(body.is_empty());
            }
            f => panic!("wrong variant: {f:?}"),
        }
    }

    #[test]
    fn ping_and_reject_decode() {
        assert!(matches!(
            ServerFrame::decode(&[FRAME_PING]).unwrap(),
            ServerFrame::Ping
        ));
        let mut buf = vec![FRAME_REJECT];
        put_str_u8(&mut buf, "rate_limited");
        buf.extend_from_slice(&60u16.to_be_bytes());
        match ServerFrame::decode(&buf).unwrap() {
            ServerFrame::Reject {
                reason,
                retry_after_secs,
            } => {
                assert_eq!(reason, "rate_limited");
                assert_eq!(retry_after_secs, 60);
            }
            f => panic!("wrong variant: {f:?}"),
        }
    }

    #[test]
    fn response_chunk_round_trips_via_layout() {
        let body = b"{\"hello\":\"world\"}";
        let encoded = ClientFrame::ResponseChunk {
            id: 7,
            last: true,
            body,
        }
        .encode();
        assert_eq!(encoded[0], FRAME_RESPONSE_CHUNK);
        assert_eq!(&encoded[1..5], &7u32.to_be_bytes());
        assert_eq!(encoded[5], FLAG_LAST);
        assert_eq!(&encoded[6..], body);
    }

    #[test]
    fn register_encodes_with_u16_len() {
        let info = br#"{"client":{"platform":"macos"}}"#;
        let encoded = ClientFrame::Register { info_json: info }.encode();
        assert_eq!(encoded[0], FRAME_REGISTER);
        let len = u16::from_be_bytes([encoded[1], encoded[2]]) as usize;
        assert_eq!(len, info.len());
        assert_eq!(&encoded[3..], &info[..]);
    }

    #[test]
    fn response_head_encodes_headers() {
        let headers = vec![("content-type".into(), "application/json".into())];
        let encoded = ClientFrame::ResponseHead {
            id: 42,
            status: 200,
            headers: &headers,
        }
        .encode();
        assert_eq!(encoded[0], FRAME_RESPONSE_HEAD);
        assert_eq!(&encoded[1..5], &42u32.to_be_bytes());
        assert_eq!(&encoded[5..7], &200u16.to_be_bytes());
        assert_eq!(encoded[7], 1); // 1 header
    }

    #[test]
    fn truncated_server_frame_errors() {
        assert!(ServerFrame::decode(&[FRAME_REGISTERED]).is_err());
        assert!(ServerFrame::decode(&[FRAME_REQUEST, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn unknown_server_frame_errors() {
        assert!(ServerFrame::decode(&[0x77]).is_err());
    }
}
