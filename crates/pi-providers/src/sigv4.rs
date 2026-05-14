//! Minimal AWS SigV4 request signer.
//!
//! Implements just enough to sign `bedrock-runtime` requests:
//!
//! - GET / POST with a string body
//! - x-amz-date / Authorization headers
//! - Region + service from caller (Bedrock uses `bedrock`)
//! - Optional STS session token
//!
//! The function returns the headers that must be attached to the outbound
//! request. We do not pull `aws-sigv4` to keep the dep graph minimal.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct SigV4Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: BTreeMap<String, String>,
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub session_token: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct SignedHeaders {
    pub headers: BTreeMap<String, String>,
}

impl SigV4Request<'_> {
    /// Produce signed headers (including `Authorization`). Date is generated
    /// from current UTC time but can be overridden via `x-amz-date` already
    /// present in `headers`.
    pub fn sign(mut self) -> SignedHeaders {
        let now = self
            .headers
            .get("x-amz-date")
            .cloned()
            .unwrap_or_else(amz_date_now);
        self.headers
            .insert("host".to_string(), self.host.to_string());
        self.headers.insert("x-amz-date".to_string(), now.clone());
        if let Some(token) = self.session_token {
            self.headers
                .insert("x-amz-security-token".to_string(), token.to_string());
        }
        let body_hash = hex_sha256(self.body);
        self.headers
            .insert("x-amz-content-sha256".to_string(), body_hash.clone());

        let canonical_headers: String = self
            .headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k.to_ascii_lowercase(), v.trim()))
            .collect();
        let signed_headers: String = self
            .headers
            .keys()
            .map(|k| k.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            uri_encode(self.path, false),
            self.query,
            canonical_headers,
            signed_headers,
            body_hash,
        );

        let date = &now[0..8];
        let credential_scope = format!("{}/{}/{}/aws4_request", date, self.region, self.service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            now,
            credential_scope,
            hex_sha256(canonical_request.as_bytes())
        );
        let signing_key = derive_signing_key(self.secret_key, date, self.region, self.service);
        let signature = hex_hmac_sha256(&signing_key, string_to_sign.as_bytes());
        let auth_header = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key, credential_scope, signed_headers, signature
        );
        self.headers
            .insert("authorization".to_string(), auth_header);
        SignedHeaders {
            headers: self.headers,
        }
    }
}

fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut key_buf = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        key_buf[..digest.len()].copy_from_slice(&digest);
    } else {
        key_buf[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key_buf[i] ^ 0x36;
        opad[i] = key_buf[i] ^ 0x5c;
    }
    let mut hasher = Sha256::new();
    hasher.update(ipad);
    hasher.update(message);
    let inner = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(opad);
    hasher.update(inner);
    hasher.finalize().to_vec()
}

fn hex_hmac_sha256(key: &[u8], message: &[u8]) -> String {
    to_hex(&hmac_sha256(key, message))
}

fn hex_sha256(input: &[u8]) -> String {
    to_hex(&Sha256::digest(input))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn amz_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (year, month, day) = days_to_ymd(days as i64);
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z")
}

fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let safe = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (byte == b'/' && !encode_slash);
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_1() {
        // RFC 4231 §4.2: key=20 bytes 0x0b, data="Hi There"
        let key = vec![0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(to_hex(&mac), expected);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn amz_date_round_trips_zeros() {
        // Just make sure format pattern compiles correctly.
        let date = amz_date_now();
        assert_eq!(date.len(), 16);
        assert!(date.ends_with('Z'));
    }

    #[test]
    fn signing_attaches_authorization_header() {
        let req = SigV4Request {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/foo/invoke",
            query: "",
            headers: BTreeMap::new(),
            body: b"{}",
            region: "us-east-1",
            service: "bedrock",
            access_key: "AKIA…",
            secret_key: "secret",
            session_token: None,
        }
        .sign();
        let auth = req.headers.get("authorization").expect("auth header");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIA…/"));
        assert!(auth.contains("SignedHeaders="));
        assert!(req.headers.get("x-amz-date").is_some());
        assert!(req
            .headers
            .get("x-amz-content-sha256")
            .map(|s| s.len() == 64)
            .unwrap_or(false));
    }
}
