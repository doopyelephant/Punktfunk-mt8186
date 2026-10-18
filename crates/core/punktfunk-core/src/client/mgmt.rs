//! The management API over a native connection: one typed management stream per request,
//! framed as the host's tunnel reads it (four bytes of length, a JSON head, the body to FIN;
//! the reply the same). The connection presents this device's certificate, so a paired device
//! reaches its lane on the stream port alone, without TCP 47990. Blocking, for the shells'
//! worker threads.

use crate::error::{PunktfunkError, Result};
use crate::quic::endpoint;
use crate::quic::v2::{io, registry};
use std::time::Duration;

/// A reply is a library page at most; a page of 200 titles is far under this.
const MAX_REPLY: usize = 8 << 20;

/// One host's management connection.
pub struct MgmtTunnel {
    rt: tokio::runtime::Runtime,
    _ep: quinn::Endpoint,
    conn: quinn::Connection,
}

/// A reply's status and body.
#[derive(Debug)]
pub struct MgmtReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl MgmtTunnel {
    /// Dial `host:port`, the stream port, presenting `identity` and pinning `pin`. A host that
    /// does not answer within `timeout` is `Timeout`; one that answers the wrong certificate is
    /// `Crypto`.
    pub fn open(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<MgmtTunnel> {
        let io_err =
            |e: &dyn std::fmt::Display| PunktfunkError::Io(std::io::Error::other(e.to_string()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (ep, conn) = rt.block_on(async {
            let addr = super::dial_addr(host, port).await?;
            let (made, observed) = endpoint::client_shared(pin, Some(identity), &[registry::ALPN]);
            let (ep, _media) = made.map_err(|e| io_err(&e))?;
            let connecting = ep.connect(addr, "punktfunk").map_err(|e| io_err(&e))?;
            match tokio::time::timeout(timeout, connecting).await {
                Err(_) => Err(PunktfunkError::Timeout),
                Ok(Ok(conn)) => Ok((ep, conn)),
                Ok(Err(e)) => {
                    let wrong_host =
                        pin.is_some() && observed.lock().unwrap().is_some_and(|fp| Some(fp) != pin);
                    Err(match wrong_host {
                        true => PunktfunkError::Crypto,
                        false => io_err(&e),
                    })
                }
            }
        })?;
        Ok(MgmtTunnel { rt, _ep: ep, conn })
    }

    /// `method` (`GET` or `POST`) `path` (`/api/v1/…` with its query) with `body`.
    pub fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<MgmtReply> {
        let io_err =
            |e: &dyn std::fmt::Display| PunktfunkError::Io(std::io::Error::other(e.to_string()));
        self.rt.block_on(async {
            let (mut tx, mut rx) = self.conn.open_bi().await.map_err(|e| io_err(&e))?;
            io::write_stream_type(&mut tx, registry::STREAM_MANAGEMENT).await?;
            let head = format!(r#"{{"m":{},"p":{}}}"#, json_str(method), json_str(path));
            let mut frame = (head.len() as u32).to_be_bytes().to_vec();
            frame.extend_from_slice(head.as_bytes());
            frame.extend_from_slice(body);
            tx.write_all(&frame).await.map_err(|e| io_err(&e))?;
            tx.finish().map_err(|e| io_err(&e))?;
            let all = rx.read_to_end(MAX_REPLY).await.map_err(|e| io_err(&e))?;
            parse_reply(&all).ok_or_else(|| io_err(&"a reply that is not one"))
        })
    }
}

/// `s` as a JSON string.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Four bytes of length, `{"s":status,"h":[…]}`, then the body. Only the status is read.
fn parse_reply(all: &[u8]) -> Option<MgmtReply> {
    let len = u32::from_be_bytes(all.get(..4)?.try_into().ok()?) as usize;
    let head = std::str::from_utf8(all.get(4..4 + len)?).ok()?;
    let at = head.find("\"s\":")? + 4;
    let digits: String = head[at..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some(MgmtReply {
        status: digits.parse().ok()?,
        body: all[4 + len..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_reply;

    #[test]
    fn a_reply_parses_and_a_short_one_does_not() {
        let head = br#"{"s":403,"h":[]}"#;
        let mut all = (head.len() as u32).to_be_bytes().to_vec();
        all.extend_from_slice(head);
        all.extend_from_slice(b"no");
        let r = parse_reply(&all).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (403, b"no".as_slice()));
        assert!(parse_reply(&all[..6]).is_none());
        assert!(parse_reply(&[0, 0]).is_none());
    }

    #[test]
    fn a_head_string_is_escaped() {
        assert_eq!(super::json_str("/a?b=\"c\\d\n"), r#""/a?b=\"c\\d\u000a""#);
    }
}
