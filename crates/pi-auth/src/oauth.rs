//! OAuth 2.1 + PKCE login flow.
//!
//! Used by `pi auth login <provider>` to obtain an access token without
//! making the user paste a long-lived API key. The flow:
//!
//! 1. Generate a 32-byte random `code_verifier`, derive `code_challenge =
//!    base64url(SHA256(verifier))`.
//! 2. Bind a `TcpListener` on `127.0.0.1:0`; the OS picks a free port.
//! 3. Build the authorization URL `(authorize?response_type=code&…
//!    redirect_uri=http://127.0.0.1:<port>/callback&code_challenge=…)`
//!    and print it. We also try to open the user's browser via
//!    `xdg-open` / `open` / `start` when `--browser` is passed.
//! 4. Block on the listener until the IDP redirects to `/callback?code=…
//!    &state=…`; verify `state` matches what we sent.
//! 5. POST `code + verifier` to the token endpoint, parse `access_token` +
//!    optional `refresh_token` + `expires_in`.
//! 6. Hand the tokens back to the caller, which stores them via the
//!    encrypted-file resolver.
//!
//! We deliberately do **not** hard-code Anthropic's client_id. Users wire
//! their own (via `PI_OAUTH_CLIENT_ID` or arg) until an official Pi Rust
//! OAuth app is registered upstream. The CLI surfaces that as a helpful
//! error when the env is missing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use pi_core::{PiError, PiErrorKind, PiResult};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub provider: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub scope: Option<String>,
    /// Path segment for the loopback redirect, default `/callback`.
    pub redirect_path: String,
    /// Interface used for the local callback listener.
    pub callback_host: String,
    /// Host name placed in the redirect URI. Some OAuth apps whitelist
    /// `localhost` exactly while we still bind the listener to `127.0.0.1`.
    pub redirect_host: String,
    /// Optional fixed callback port for providers with a pre-registered URI.
    pub callback_port: Option<u16>,
    /// Provider-specific authorization query parameters.
    pub extra_authorize_params: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    /// Absolute expiry timestamp (seconds since UNIX epoch). Populated by
    /// `run` and `refresh`; older serialized tokens omit it which we treat
    /// as "expires unknown — refresh on first 401".
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
}

impl OAuthTokens {
    /// Whether the token is past (or close to) its expiry. We rebuild access
    /// tokens when fewer than 60 seconds remain to avoid races with the
    /// upstream clock.
    pub fn needs_refresh(&self) -> bool {
        match self.expires_at_unix {
            Some(at) => current_unix() + 60 >= at,
            None => false,
        }
    }
}

fn current_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// OAuth configuration used by the upstream TypeScript `pi` implementation for
/// ChatGPT Plus/Pro Codex subscription access.
pub fn openai_codex_config() -> OAuthConfig {
    OAuthConfig {
        provider: "openai-codex".to_string(),
        authorize_endpoint: "https://auth.openai.com/oauth/authorize".to_string(),
        token_endpoint: "https://auth.openai.com/oauth/token".to_string(),
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
        scope: Some("openid profile email offline_access".to_string()),
        redirect_path: "/auth/callback".to_string(),
        callback_host: std::env::var("PI_OAUTH_CALLBACK_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string()),
        redirect_host: "localhost".to_string(),
        callback_port: Some(1455),
        extra_authorize_params: vec![
            ("id_token_add_organizations".to_string(), "true".to_string()),
            ("codex_cli_simplified_flow".to_string(), "true".to_string()),
            ("originator".to_string(), "pi".to_string()),
        ],
    }
}

/// Extract the ChatGPT account id required by the Codex backend from the OAuth
/// access token's JWT payload.
pub fn openai_codex_account_id(access_token: &str) -> PiResult<String> {
    let mut parts = access_token.split('.');
    let _header = parts.next();
    let payload = parts.next().ok_or_else(|| {
        PiError::new(
            PiErrorKind::Provider,
            "OpenAI Codex access token 不是有效 JWT",
        )
    })?;
    if parts.next().is_none() {
        return Err(PiError::new(
            PiErrorKind::Provider,
            "OpenAI Codex access token 不是有效 JWT",
        ));
    }
    let bytes = base64url_decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("解析 OpenAI Codex access token 失败：{err}"),
        )
    })?;
    value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                "OpenAI Codex access token 缺少 chatgpt_account_id",
            )
        })
}

/// Run the full PKCE flow synchronously and return the resulting tokens.
/// `timeout` caps how long we wait for the browser callback.
pub fn run(config: &OAuthConfig, timeout: Duration, open_browser: bool) -> PiResult<OAuthTokens> {
    let bind_addr = format!(
        "{}:{}",
        config.callback_host,
        config.callback_port.unwrap_or(0)
    );
    let listener = TcpListener::bind(&bind_addr)
        .map_err(|err| PiError::new(PiErrorKind::Io, format!("无法绑定本地回调端口：{err}")))?;
    let addr = listener
        .local_addr()
        .map_err(|err| PiError::new(PiErrorKind::Io, format!("读取本地端口失败：{err}")))?;
    let redirect_uri = format!(
        "http://{}:{}{}",
        config.redirect_host,
        addr.port(),
        config.redirect_path
    );

    let state = random_token(24);
    let verifier = random_token(64);
    let challenge = pkce_challenge(&verifier);

    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        config.authorize_endpoint,
        urlencode(&config.client_id),
        urlencode(&redirect_uri),
        urlencode(&state),
        urlencode(&challenge),
    );
    if let Some(scope) = &config.scope {
        url.push_str("&scope=");
        url.push_str(&urlencode(scope));
    }
    for (key, value) in &config.extra_authorize_params {
        url.push('&');
        url.push_str(&urlencode(key));
        url.push('=');
        url.push_str(&urlencode(value));
    }

    eprintln!(
        "请在浏览器中访问以下 URL 完成 {} 登录：\n{url}\n",
        config.provider
    );
    if open_browser {
        let _ = try_open_browser(&url);
    }

    let deadline = Instant::now() + timeout;
    listener
        .set_nonblocking(true)
        .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?;

    let mut received: Option<(String, String)> = None;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(params) = handle_callback(stream, &config.redirect_path)? {
                    received = Some(params);
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(PiError::new(
                    PiErrorKind::Io,
                    format!("接受回调连接失败：{err}"),
                ));
            }
        }
    }

    let (code, returned_state) = received.ok_or_else(|| {
        PiError::new(
            PiErrorKind::Cancelled,
            format!("等待 {} OAuth 回调超时", config.provider),
        )
    })?;
    if returned_state != state {
        return Err(PiError::new(
            PiErrorKind::Provider,
            "OAuth state 不匹配，可能存在 CSRF",
        ));
    }
    exchange_code(config, &code, &verifier, &redirect_uri)
}

fn handle_callback(
    mut stream: TcpStream,
    expected_path: &str,
) -> PiResult<Option<(String, String)>> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?,
    );
    let mut first = String::new();
    reader
        .read_line(&mut first)
        .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?;
    let path = first.split_whitespace().nth(1).unwrap_or("");
    if !path.starts_with(expected_path) {
        write_simple_response(
            &mut stream,
            "404 Not Found",
            "pi-rust OAuth callback expected a different path",
        )?;
        return Ok(None);
    }
    // Drain remaining headers so the client thinks we read the request.
    loop {
        let mut buf = String::new();
        if reader
            .read_line(&mut buf)
            .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?
            == 0
        {
            break;
        }
        if buf == "\r\n" || buf == "\n" {
            break;
        }
    }
    let qs = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut params: HashMap<String, String> = HashMap::new();
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            params.insert(k.to_string(), urldecode(v));
        }
    }
    write_simple_response(
        &mut stream,
        "200 OK",
        "Pi Rust OAuth callback received. You can close this tab.",
    )?;
    if let Some(error) = params.get("error") {
        return Err(PiError::new(
            PiErrorKind::Provider,
            format!("OAuth 错误：{error}"),
        ));
    }
    let code = params.get("code").cloned();
    let state = params.get("state").cloned();
    match (code, state) {
        (Some(c), Some(s)) => Ok(Some((c, s))),
        _ => Ok(None),
    }
}

fn write_simple_response(stream: &mut TcpStream, status: &str, body: &str) -> PiResult<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?;
    Ok(())
}

fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> PiResult<OAuthTokens> {
    let body = format!(
        "grant_type=authorization_code&client_id={}&code={}&redirect_uri={}&code_verifier={}",
        urlencode(&config.client_id),
        urlencode(code),
        urlencode(redirect_uri),
        urlencode(verifier),
    );
    let mut tokens = post_token_request(&config.token_endpoint, &body)?;
    stamp_expiry(&mut tokens);
    Ok(tokens)
}

/// Exchange a `refresh_token` for a fresh `access_token`. Returns the new
/// tokens (which usually keep the same `refresh_token` but providers MAY
/// rotate it; we preserve the new one when present).
pub fn refresh(config: &OAuthConfig, refresh_token: &str) -> PiResult<OAuthTokens> {
    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        urlencode(&config.client_id),
        urlencode(refresh_token),
    );
    let mut tokens = post_token_request(&config.token_endpoint, &body)?;
    if tokens.refresh_token.is_none() {
        // Most IdPs omit `refresh_token` from the refresh response; keep the
        // old one so the caller can refresh again later.
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    stamp_expiry(&mut tokens);
    Ok(tokens)
}

fn post_token_request(token_endpoint: &str, body: &str) -> PiResult<OAuthTokens> {
    let agent = ureq_shim::agent();
    let response = match agent
        .post(token_endpoint)
        .set("content-type", "application/x-www-form-urlencoded")
        .send_string(body)
    {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(PiError::new(
                PiErrorKind::Provider,
                format!("token 交换失败 HTTP {status}：{body}"),
            ));
        }
        Err(ureq::Error::Transport(err)) => {
            return Err(PiError::new(
                PiErrorKind::Network,
                format!("token 交换失败：{err}"),
            ));
        }
    };
    let text = response
        .into_string()
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("读取 token 响应失败：{err}")))?;
    let tokens: OAuthTokens = serde_json::from_str(&text).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("解析 token 响应失败：{err}; body={text}"),
        )
    })?;
    Ok(tokens)
}

fn stamp_expiry(tokens: &mut OAuthTokens) {
    if let Some(expires_in) = tokens.expires_in {
        tokens.expires_at_unix = Some(current_unix().saturating_add(expires_in));
    }
}

mod ureq_shim {
    pub fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(30))
            .timeout_read(std::time::Duration::from_secs(60))
            .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION"), " oauth"))
            .build()
    }
}

fn try_open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "start";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd).arg(url).spawn().map(|_| ())
}

fn random_token(len: usize) -> String {
    // PKCE verifier must be 43..128 unreserved chars. We use base64url
    // alphabet on bytes from /dev/urandom (with a deterministic fallback
    // matching the telemetry id generator's strategy).
    use std::fs;
    use std::io::Read;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = vec![0u8; len];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    } else {
        let mix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((mix >> ((i * 7) % 128)) & 0xff) as u8;
        }
    }
    bytes
        .iter()
        .map(|b| ALPHABET[(b & 0x3f) as usize] as char)
        .collect()
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url(&digest)
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let chunk =
            ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(chunk & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let chunk = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let chunk = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
    }
    out
}

fn base64url_decode(input: &str) -> PiResult<Vec<u8>> {
    fn val(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            b'=' => None,
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i < bytes.len() {
        let remaining = bytes.len() - i;
        let take = remaining.min(4);
        if take == 1 {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "无效 base64url JWT payload",
            ));
        }
        let mut chunk = [0u8; 4];
        for j in 0..take {
            chunk[j] = val(bytes[i + j])
                .ok_or_else(|| PiError::new(PiErrorKind::Provider, "无效 base64url JWT payload"))?;
        }
        let triple = ((chunk[0] as u32) << 18)
            | ((chunk[1] as u32) << 12)
            | ((chunk[2] as u32) << 6)
            | (chunk[3] as u32);
        out.push(((triple >> 16) & 0xff) as u8);
        if take >= 3 {
            out.push(((triple >> 8) & 0xff) as u8);
        }
        if take == 4 {
            out.push((triple & 0xff) as u8);
        }
        i += take;
    }
    Ok(out)
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi as u8 * 16 + lo as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_test_vector() {
        // RFC 7636 Appendix B reference vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_token_is_within_pkce_length() {
        let t = random_token(64);
        assert_eq!(t.len(), 64);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("a b/c?d"), "a%20b%2Fc%3Fd");
    }

    #[test]
    fn urldecode_round_trips() {
        assert_eq!(urldecode("a%20b%2Fc"), "a b/c");
    }

    #[test]
    fn base64url_encodes_known_value() {
        assert_eq!(base64url(&[0xff, 0xff, 0xff]), "____");
        assert_eq!(base64url(b"hi"), "aGk");
    }

    #[test]
    fn codex_config_matches_upstream_oauth_app() {
        let config = openai_codex_config();
        assert_eq!(config.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(config.redirect_path, "/auth/callback");
        assert_eq!(config.redirect_host, "localhost");
        assert_eq!(config.callback_port, Some(1455));
        assert!(config
            .extra_authorize_params
            .iter()
            .any(|(key, value)| key == "codex_cli_simplified_flow" && value == "true"));
    }

    #[test]
    fn extracts_codex_account_id_from_access_token() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123"
            }
        });
        let token = format!(
            "{}.{}.sig",
            base64url(br#"{"alg":"none"}"#),
            base64url(payload.to_string().as_bytes())
        );
        let account_id = openai_codex_account_id(&token).expect("account id");
        assert_eq!(account_id, "acct_123");
    }

    #[test]
    fn needs_refresh_returns_true_just_before_expiry() {
        let now = current_unix();
        let tokens = OAuthTokens {
            access_token: "x".into(),
            refresh_token: Some("r".into()),
            token_type: None,
            expires_in: Some(60),
            scope: None,
            expires_at_unix: Some(now + 30), // 30s away → inside the 60s buffer
        };
        assert!(tokens.needs_refresh());
    }

    #[test]
    fn needs_refresh_returns_false_when_far_from_expiry() {
        let now = current_unix();
        let tokens = OAuthTokens {
            access_token: "x".into(),
            refresh_token: Some("r".into()),
            token_type: None,
            expires_in: Some(3600),
            scope: None,
            expires_at_unix: Some(now + 3000),
        };
        assert!(!tokens.needs_refresh());
    }

    #[test]
    fn stamp_expiry_writes_expires_at_unix_when_expires_in_present() {
        let mut tokens = OAuthTokens {
            access_token: "x".into(),
            refresh_token: None,
            token_type: None,
            expires_in: Some(120),
            scope: None,
            expires_at_unix: None,
        };
        stamp_expiry(&mut tokens);
        let now = current_unix();
        let stamped = tokens.expires_at_unix.expect("expiry");
        assert!(stamped >= now + 110 && stamped <= now + 130);
    }
}
