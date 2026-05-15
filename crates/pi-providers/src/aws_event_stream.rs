//! AWS `application/vnd.amazon.eventstream` binary framing parser.
//!
//! Used by Bedrock's `invokeModelWithResponseStream`. Each message on the
//! wire has the layout:
//!
//! ```text
//! +---------------------------+
//! | total length (4 BE)       |  prelude + headers + payload + msg crc
//! +---------------------------+
//! | headers length (4 BE)     |
//! +---------------------------+
//! | prelude crc32 (4 BE)      |  crc32 of total length + headers length
//! +---------------------------+
//! | headers (variable)        |  name-len(1) | name | type(1) | value
//! +---------------------------+
//! | payload (variable)        |  usually JSON ({"bytes":"<base64>"} on Bedrock)
//! +---------------------------+
//! | message crc32 (4 BE)      |  crc32 of everything before the msg crc
//! +---------------------------+
//! ```
//!
//! We implement just enough header decoding to read `:message-type` and
//! `:event-type` (both string-typed); other types are skipped. CRC errors
//! abort the stream — every message is independently verifiable.
//!
//! Reference: AWS event-stream protocol spec (open-sourced inside the AWS
//! SDK for JavaScript, `@aws-sdk/eventstream-codec`).

use std::io::Read;

use pi_core::{PiError, PiErrorKind, PiResult};

/// One decoded event from the binary wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventStreamMessage {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl EventStreamMessage {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Streaming parser. Calls `on_message` for every decoded message until the
/// reader is exhausted or `on_message` returns an error.
pub fn parse<R, F>(mut reader: R, mut on_message: F) -> PiResult<()>
where
    R: Read,
    F: FnMut(EventStreamMessage) -> PiResult<()>,
{
    let mut prelude = [0u8; 12];
    loop {
        // Peek for EOF: try reading the first byte of the prelude.
        let read_first = read_exact_or_eof(&mut reader, &mut prelude[..1])?;
        if !read_first {
            return Ok(());
        }
        // Read the remaining 11 bytes of the prelude.
        reader.read_exact(&mut prelude[1..]).map_err(|err| {
            PiError::new(
                PiErrorKind::Provider,
                format!("event-stream prelude 读取失败：{err}"),
            )
        })?;
        let total_len = u32::from_be_bytes([prelude[0], prelude[1], prelude[2], prelude[3]]);
        let headers_len = u32::from_be_bytes([prelude[4], prelude[5], prelude[6], prelude[7]]);
        let prelude_crc = u32::from_be_bytes([prelude[8], prelude[9], prelude[10], prelude[11]]);
        let want_prelude_crc = crc32(&prelude[..8]);
        if prelude_crc != want_prelude_crc {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "event-stream prelude CRC32 校验失败",
            ));
        }
        if (total_len as usize) < 16 {
            return Err(PiError::new(
                PiErrorKind::Provider,
                format!("event-stream total_len 太小：{total_len}"),
            ));
        }
        let remaining = (total_len as usize) - 12;
        let mut rest = vec![0u8; remaining];
        reader.read_exact(&mut rest).map_err(|err| {
            PiError::new(
                PiErrorKind::Provider,
                format!("event-stream 消息读取失败：{err}"),
            )
        })?;
        // Layout in `rest`: [headers][payload][msg_crc(4 BE)]
        if rest.len() < 4 + headers_len as usize {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "event-stream headers 长度越界",
            ));
        }
        let msg_crc_offset = rest.len() - 4;
        let msg_crc = u32::from_be_bytes([
            rest[msg_crc_offset],
            rest[msg_crc_offset + 1],
            rest[msg_crc_offset + 2],
            rest[msg_crc_offset + 3],
        ]);
        // CRC covers prelude (12 bytes) + headers + payload.
        let want_msg_crc = {
            let mut crc = crc32(&prelude);
            crc = crc32_continue(crc, &rest[..msg_crc_offset]);
            crc
        };
        if msg_crc != want_msg_crc {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "event-stream message CRC32 校验失败",
            ));
        }
        let headers = decode_headers(&rest[..headers_len as usize])?;
        let payload = rest[headers_len as usize..msg_crc_offset].to_vec();
        on_message(EventStreamMessage { headers, payload })?;
    }
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> PiResult<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Ok(filled > 0);
            }
            Ok(n) => filled += n,
            Err(err) => {
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("event-stream IO 错误：{err}"),
                ));
            }
        }
    }
    Ok(true)
}

fn decode_headers(buf: &[u8]) -> PiResult<Vec<(String, String)>> {
    let mut headers = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf.len() - i < 2 {
            break;
        }
        let name_len = buf[i] as usize;
        i += 1;
        if i + name_len > buf.len() {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "event-stream header name 越界",
            ));
        }
        let name = std::str::from_utf8(&buf[i..i + name_len])
            .map_err(|err| {
                PiError::new(
                    PiErrorKind::Provider,
                    format!("event-stream header name 非 UTF-8：{err}"),
                )
            })?
            .to_string();
        i += name_len;
        if i >= buf.len() {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "event-stream header value type 缺失",
            ));
        }
        let value_type = buf[i];
        i += 1;
        let value = match value_type {
            7 => {
                if i + 2 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header value length 越界",
                    ));
                }
                let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                i += 2;
                if i + len > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header value 越界",
                    ));
                }
                let value = std::str::from_utf8(&buf[i..i + len])
                    .map_err(|err| {
                        PiError::new(
                            PiErrorKind::Provider,
                            format!("event-stream header value 非 UTF-8：{err}"),
                        )
                    })?
                    .to_string();
                i += len;
                value
            }
            // Other types (bool, byte, int16/32/64, byte_array, uuid, timestamp).
            // We only need string headers for Bedrock event routing; skip the rest.
            0 => String::from("true"),
            1 => String::from("false"),
            2 => {
                if i + 1 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header byte 越界",
                    ));
                }
                let v = buf[i];
                i += 1;
                v.to_string()
            }
            3 => {
                if i + 2 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header int16 越界",
                    ));
                }
                let v = i16::from_be_bytes([buf[i], buf[i + 1]]);
                i += 2;
                v.to_string()
            }
            4 => {
                if i + 4 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header int32 越界",
                    ));
                }
                let v = i32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
                i += 4;
                v.to_string()
            }
            5 => {
                if i + 8 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header int64 越界",
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&buf[i..i + 8]);
                let v = i64::from_be_bytes(bytes);
                i += 8;
                v.to_string()
            }
            6 => {
                // byte array — represent as hex string.
                if i + 2 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header byte-array 长度越界",
                    ));
                }
                let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
                i += 2;
                if i + len > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header byte-array 内容越界",
                    ));
                }
                let mut hex = String::with_capacity(len * 2);
                for b in &buf[i..i + len] {
                    use std::fmt::Write as _;
                    let _ = write!(hex, "{b:02x}");
                }
                i += len;
                hex
            }
            8 => {
                // timestamp (int64 milliseconds since epoch)
                if i + 8 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header timestamp 越界",
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&buf[i..i + 8]);
                let v = i64::from_be_bytes(bytes);
                i += 8;
                v.to_string()
            }
            9 => {
                // uuid — 16 bytes hex
                if i + 16 > buf.len() {
                    return Err(PiError::new(
                        PiErrorKind::Provider,
                        "event-stream header uuid 越界",
                    ));
                }
                let mut hex = String::with_capacity(32);
                for b in &buf[i..i + 16] {
                    use std::fmt::Write as _;
                    let _ = write!(hex, "{b:02x}");
                }
                i += 16;
                hex
            }
            _ => {
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("event-stream header 未知类型 {value_type}"),
                ));
            }
        };
        headers.push((name, value));
    }
    Ok(headers)
}

// Standard CRC-32 (IEEE 802.3 / PKZIP). Polynomial reflected as 0xEDB88320.

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut j = 0;
        while j < 8 {
            if (c & 1) != 0 {
                c = 0xEDB88320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
            j += 1;
        }
        table[i as usize] = c;
        i += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    crc32_continue(0, bytes)
}

fn crc32_continue(seed: u32, bytes: &[u8]) -> u32 {
    let mut c = seed ^ 0xFFFF_FFFF;
    for &b in bytes {
        c = CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        // Encode headers as name-len(1) | name | type(7 = string) | val-len(2) | val
        let mut hdr_buf: Vec<u8> = Vec::new();
        for (name, value) in headers {
            assert!(name.len() <= u8::MAX as usize);
            hdr_buf.push(name.len() as u8);
            hdr_buf.extend_from_slice(name.as_bytes());
            hdr_buf.push(7);
            let len = value.len() as u16;
            hdr_buf.extend_from_slice(&len.to_be_bytes());
            hdr_buf.extend_from_slice(value.as_bytes());
        }
        let headers_len = hdr_buf.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;
        let mut prelude = Vec::with_capacity(12);
        prelude.extend_from_slice(&total_len.to_be_bytes());
        prelude.extend_from_slice(&headers_len.to_be_bytes());
        let prelude_crc = crc32(&prelude);
        prelude.extend_from_slice(&prelude_crc.to_be_bytes());

        let mut msg = Vec::new();
        msg.extend_from_slice(&prelude);
        msg.extend_from_slice(&hdr_buf);
        msg.extend_from_slice(payload);
        let msg_crc = crc32(&msg);
        msg.extend_from_slice(&msg_crc.to_be_bytes());
        msg
    }

    #[test]
    fn round_trip_single_message() {
        let bytes = encode_message(
            &[(":message-type", "event"), (":event-type", "chunk")],
            br#"{"bytes":"ZXhhbXBsZQ=="}"#,
        );
        let mut seen = Vec::new();
        parse(std::io::Cursor::new(bytes), |msg| {
            seen.push(msg);
            Ok(())
        })
        .expect("parse");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].header(":message-type"), Some("event"));
        assert_eq!(seen[0].header(":event-type"), Some("chunk"));
        assert_eq!(seen[0].payload, br#"{"bytes":"ZXhhbXBsZQ=="}"#);
    }

    #[test]
    fn round_trip_multiple_messages() {
        let m1 = encode_message(&[(":event-type", "first")], b"alpha");
        let m2 = encode_message(&[(":event-type", "second")], b"beta");
        let m3 = encode_message(&[(":event-type", "third")], b"gamma");
        let mut combined = Vec::new();
        combined.extend_from_slice(&m1);
        combined.extend_from_slice(&m2);
        combined.extend_from_slice(&m3);
        let mut events = Vec::new();
        parse(std::io::Cursor::new(combined), |msg| {
            events.push(msg.header(":event-type").unwrap_or_default().to_string());
            Ok(())
        })
        .expect("parse");
        assert_eq!(events, vec!["first", "second", "third"]);
    }

    #[test]
    fn corrupt_prelude_crc_is_rejected() {
        let mut bytes = encode_message(&[(":event-type", "first")], b"data");
        bytes[10] ^= 0x40; // flip a bit in the prelude crc
        let result = parse(std::io::Cursor::new(bytes), |_| Ok(()));
        assert!(matches!(result, Err(err) if err.message.contains("prelude CRC32")));
    }

    #[test]
    fn corrupt_message_crc_is_rejected() {
        let mut bytes = encode_message(&[(":event-type", "first")], b"data");
        let len = bytes.len();
        bytes[len - 1] ^= 0xFF;
        let result = parse(std::io::Cursor::new(bytes), |_| Ok(()));
        assert!(matches!(result, Err(err) if err.message.contains("message CRC32")));
    }

    #[test]
    fn crc32_matches_known_vector() {
        // RFC 1952 test vector: CRC-32 of "123456789" = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
