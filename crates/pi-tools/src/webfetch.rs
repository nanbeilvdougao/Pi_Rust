//! `webfetch` tool — issues an HTTPS GET and returns a markdown digest of
//! the response.
//!
//! Responsibilities:
//!
//! - Gate on `Capability::Network` so the permission engine sees the call.
//! - Cap response size (`max_bytes`, default 1 MiB) so a hostile or huge
//!   resource cannot exhaust memory.
//! - Honor a short timeout (`timeout_ms`, default 15 000) — the user can
//!   raise it but never beyond `MAX_TIMEOUT_MS = 60_000`.
//! - When the response is HTML, strip scripts/styles and collapse whitespace
//!   into a markdown-ish skeleton so the model receives readable text.
//!   Other content types come back verbatim (with a guard against binary
//!   garbage spilling into the context window).
//!
//! We deliberately do **not** parse robots.txt here — the agent should
//! check robots.txt itself if compliance is required. Adding silent
//! enforcement would be surprising and untestable.
//!
//! Parity target: `packages/agent/src/tools/webfetch.ts`.

use std::io::Read;
use std::time::Duration;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct WebFetchTool;

#[derive(Debug, Deserialize, Default)]
struct WebFetchInput {
    url: String,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    accept: Option<String>,
}

const DEFAULT_MAX_BYTES: u64 = 1 << 20; // 1 MiB
const MAX_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_TIMEOUT_MS: u64 = 15_000;

impl Tool for WebFetchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "webfetch".to_string(),
            description: "通过 HTTPS GET 抓取一个 URL，HTML 会被简化成 markdown 摘要"
                .to_string(),
            input_shape: "json".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "完整 https URL"},
                    "max_bytes": {"type": "integer", "minimum": 0, "default": DEFAULT_MAX_BYTES},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_TIMEOUT_MS, "default": DEFAULT_TIMEOUT_MS},
                    "accept": {"type": "string", "description": "可选 Accept 头"}
                },
                "required": ["url"],
                "additionalProperties": false
            })),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: WebFetchInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            WebFetchInput {
                url: input.raw.clone(),
                ..WebFetchInput::default()
            }
        };
        let url = parsed.url.trim().to_string();
        if url.is_empty() {
            return Err(PiError::new(PiErrorKind::InvalidInput, "webfetch url 不能为空"));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "webfetch 仅接受 http:// / https:// 协议",
            ));
        }
        permissions.require(PermissionRequest {
            capability: Capability::Network,
            target: url.clone(),
            reason: format!("HTTP GET {url}"),
        })?;
        let max_bytes = parsed.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        let timeout_ms = parsed
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(timeout_ms))
            .timeout_read(Duration::from_millis(timeout_ms))
            .timeout_write(Duration::from_millis(timeout_ms))
            .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION"), " webfetch"))
            .redirects(5)
            .build();
        let mut request = agent.get(&url);
        if let Some(accept) = &parsed.accept {
            request = request.set("accept", accept);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(PiError::new(
                    PiErrorKind::Network,
                    format!("webfetch HTTP {status}：{body}"),
                ));
            }
            Err(ureq::Error::Transport(err)) => {
                return Err(PiError::new(
                    PiErrorKind::Network,
                    format!("webfetch 传输错误：{err}"),
                ));
            }
        };
        let status = response.status();
        let content_type = response
            .header("content-type")
            .unwrap_or("application/octet-stream")
            .to_string();
        let final_url = response.get_url().to_string();
        let reader = response.into_reader().take(max_bytes + 1);
        let mut bytes: Vec<u8> = Vec::new();
        reader
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| {
                PiError::new(PiErrorKind::Io, format!("webfetch 读取失败：{err}"))
            })?;
        let truncated = bytes.len() as u64 > max_bytes;
        if bytes.len() as u64 > max_bytes {
            bytes.truncate(max_bytes as usize);
        }

        let body = if looks_like_text(&content_type, &bytes) {
            String::from_utf8_lossy(&bytes).to_string()
        } else {
            return Ok(ToolOutput {
                name: "webfetch".to_string(),
                output: format!(
                    "url: {final_url}\nstatus: {status}\ncontent-type: {content_type}\n(binary body, {} 字节，未返回原文)",
                    bytes.len()
                ),
            });
        };

        let rendered = if content_type.contains("html") {
            html_to_markdown(&body)
        } else {
            body
        };
        let suffix = if truncated {
            format!("\n\n…[已截断到 {max_bytes} 字节]")
        } else {
            String::new()
        };
        Ok(ToolOutput {
            name: "webfetch".to_string(),
            output: format!(
                "url: {final_url}\nstatus: {status}\ncontent-type: {content_type}\n\n{rendered}{suffix}"
            ),
        })
    }
}

fn looks_like_text(content_type: &str, body: &[u8]) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("xml")
        || ct.contains("yaml")
        || ct.contains("csv")
    {
        return true;
    }
    // Heuristic: <10% non-printable bytes in the first 4 KiB.
    let sample_len = body.len().min(4096);
    if sample_len == 0 {
        return true;
    }
    let bad = body[..sample_len]
        .iter()
        .filter(|b| **b != b'\n' && **b != b'\r' && **b != b'\t' && (**b < 0x20 || **b > 0x7E))
        .count();
    bad * 10 < sample_len
}

/// Minimal HTML → markdown-ish converter:
/// - Drops `<script>` / `<style>` blocks entirely.
/// - Strips remaining tags but preserves headings, anchors, list bullets.
/// - Collapses runs of whitespace into a single space; newline between blocks.
pub(crate) fn html_to_markdown(html: &str) -> String {
    let stripped = drop_blocks(html, &["script", "style", "noscript", "svg"]);
    let mut out = String::with_capacity(stripped.len() / 2);
    let bytes = stripped.as_bytes();
    let mut i = 0;
    let mut text_buf = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'<' {
            flush_text(&mut text_buf, &mut out);
            // Walk to '>'
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            let tag = std::str::from_utf8(&bytes[i + 1..j.min(bytes.len())])
                .unwrap_or("")
                .trim();
            let lower = tag.to_ascii_lowercase();
            if lower.starts_with("h1") {
                out.push_str("\n\n# ");
            } else if lower.starts_with("h2") {
                out.push_str("\n\n## ");
            } else if lower.starts_with("h3") {
                out.push_str("\n\n### ");
            } else if lower.starts_with("h4") {
                out.push_str("\n\n#### ");
            } else if lower.starts_with("li") {
                out.push_str("\n- ");
            } else if lower.starts_with("br") {
                out.push('\n');
            } else if lower == "/p" || lower.starts_with("/h") || lower == "/li" {
                out.push('\n');
            } else if lower.starts_with("p") {
                out.push_str("\n\n");
            }
            i = j + 1;
        } else {
            text_buf.push(c as char);
            i += 1;
        }
    }
    flush_text(&mut text_buf, &mut out);
    let mut compact = String::with_capacity(out.len());
    let mut prev_newline = 0u8;
    for ch in out.chars() {
        if ch == '\n' {
            prev_newline = prev_newline.saturating_add(1);
            if prev_newline <= 2 {
                compact.push(ch);
            }
        } else {
            prev_newline = 0;
            compact.push(ch);
        }
    }
    decode_html_entities(compact.trim()).to_string()
}

fn flush_text(buf: &mut String, out: &mut String) {
    let trimmed: String = buf
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !trimmed.is_empty() {
        if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&trimmed);
    }
    buf.clear();
}

fn drop_blocks(html: &str, tags: &[&str]) -> String {
    let mut text = html.to_string();
    for tag in tags {
        loop {
            let lower = text.to_ascii_lowercase();
            let open = format!("<{tag}");
            let close = format!("</{tag}>");
            let Some(start) = lower.find(&open) else {
                break;
            };
            let Some(end_rel) = lower[start..].find(&close) else {
                // unterminated block — drop to end.
                text.truncate(start);
                break;
            };
            let end = start + end_rel + close.len();
            text.replace_range(start..end, " ");
        }
    }
    text
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_marks_tool_as_non_mutating() {
        let schema = WebFetchTool.schema();
        assert_eq!(schema.name, "webfetch");
        assert!(!schema.mutates);
    }

    #[test]
    fn html_to_markdown_drops_scripts_and_styles() {
        let html = r#"<html><head><style>body { color: red; }</style></head>
        <body><h1>Title</h1><p>Hello <b>world</b></p>
        <script>alert(1)</script><ul><li>one</li><li>two</li></ul></body></html>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Hello world"));
        assert!(md.contains("- one"));
        assert!(md.contains("- two"));
        assert!(!md.contains("alert(1)"));
        assert!(!md.contains("color: red"));
    }

    #[test]
    fn html_entities_decoded() {
        assert_eq!(html_to_markdown("<p>a &amp; b</p>"), "a & b");
        assert_eq!(html_to_markdown("&lt;tag&gt;"), "<tag>");
    }

    #[test]
    fn binary_payload_is_not_returned_verbatim() {
        let mostly_binary = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 0xff, 0xfe, 0xfd];
        assert!(!looks_like_text("application/octet-stream", &mostly_binary));
    }

    #[test]
    fn json_content_type_is_text() {
        assert!(looks_like_text("application/json", b"{\"a\":1}"));
    }
}
