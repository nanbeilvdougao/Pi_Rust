//! Read sessions written by TS pi (`packages/coding-agent`).
//!
//! TS pi's session JSONL is structurally different from ours:
//!
//! ```jsonc
//! {"type":"session","version":3,"id":"...","timestamp":"2026-…","cwd":"…","parentSession":null}
//! {"type":"message","id":"…","parentId":null,"timestamp":"…","message":{"role":"user","content":[...]}}
//! {"type":"compaction","summary":"…","firstKeptEntryId":"…","tokensBefore":12345}
//! ```
//!
//! We accept these shapes as an alternate dialect of our own JSONL. The
//! returned `Session` only carries the parts our agent loop knows what to
//! do with:
//!
//! - `SessionHeader` v ≥ 2 (we map TS `timestamp` string into our
//!   `created_ms` epoch-millis where possible).
//! - `SessionMessageEntry` → `Message`. Content arrays are flattened into a
//!   single text body so the agent can still use them, with `[text]`,
//!   `[tool_use]`, `[tool_result]` blocks preserved as serialized JSON.
//! - Everything else (`compaction`, `label`, `model_change`, `custom`, …) is
//!   skipped so reloading a TS session does not surface noise to the agent;
//!   the lines remain on disk untouched.
//!
//! This is read-only: when our agent writes new turns into a TS-format
//! session, it appends our native JSON lines. Both dialects coexist; the
//! loader handles each line independently.

use pi_core::{Message, Role};
use serde::Deserialize;
use serde_json::Value;

use crate::SessionHeader;

#[derive(Debug, Deserialize)]
struct TsHeaderRaw {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "parentSession")]
    parent_session: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TsEntryRaw {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<TsMessage>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, rename = "toolCallId")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TsMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default, rename = "tool_call_id")]
    tool_call_id: Option<String>,
}

/// Try to parse `line` as a TS-pi session header. Returns `None` if the line
/// is not a TS header (caller falls back to our native or legacy paths).
pub fn parse_ts_header(line: &str) -> Option<SessionHeader> {
    let parsed: TsHeaderRaw = serde_json::from_str(line).ok()?;
    if parsed.kind != "session" {
        return None;
    }
    // Reject "looks like our own header" — ours uses `created_ms` (a number)
    // instead of `timestamp` (a string), and never carries `parentSession`.
    if !line.contains("\"timestamp\"") && !line.contains("\"parentSession\"") {
        return None;
    }
    Some(SessionHeader {
        kind: "session".to_string(),
        version: parsed.version.unwrap_or(3),
        id: parsed.id.unwrap_or_default(),
        created_ms: parsed
            .timestamp
            .as_deref()
            .and_then(parse_rfc3339_ms)
            .unwrap_or(0),
        cwd: parsed.cwd,
        parent_session: parsed.parent_session,
        title: None,
        provider: None,
        model: None,
    })
}

/// Try to parse `line` as a TS-pi session entry. Returns `Some(Message)` for
/// `type:"message"` entries that map cleanly; returns `None` for entries we
/// chose to skip (compaction, label, custom, …) so the caller can keep
/// walking the file.
pub fn parse_ts_entry(line: &str) -> Option<Message> {
    let parsed: TsEntryRaw = serde_json::from_str(line).ok()?;
    if parsed.kind != "message" {
        return None;
    }
    let message = parsed.message?;
    let role = match message.role.as_deref()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        _ => return None,
    };
    let content = match message.content {
        Some(Value::String(s)) => s,
        Some(Value::Array(parts)) => flatten_content_array(&parts),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let mut out = Message::new(role, content);
    out.tool_call_id = message.tool_call_id.or(parsed.tool_call_id);
    if let Some(timestamp) = parsed.timestamp.as_deref() {
        if let Some(ms) = parse_rfc3339_ms(timestamp) {
            out.timestamp_ms = ms;
        }
    }
    Some(out)
}

fn flatten_content_array(parts: &[Value]) -> String {
    let mut out = String::new();
    for part in parts {
        match part.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                }
            }
            Some("tool_use") => {
                let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let input = part.get("input").cloned().unwrap_or(Value::Null);
                out.push_str(&format!("[tool_use name={name} id={id} input={input}]"));
            }
            Some("tool_result") => {
                let id = part
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let body = part
                    .get("content")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                out.push_str(&format!("[tool_result id={id} {body}]"));
            }
            Some("image") => {
                out.push_str("[image]");
            }
            _ => {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Parse an RFC3339 timestamp into epoch milliseconds. We do a small custom
/// parse instead of pulling chrono — the format is fixed-width.
fn parse_rfc3339_ms(s: &str) -> Option<u128> {
    // Expected like "2026-05-14T03:42:11.123Z" or "...+00:00".
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    let fractional: u128 = s
        .get(19..)
        .and_then(|tail| {
            let tail = tail.trim_end_matches('Z');
            let dot = tail.strip_prefix('.')?;
            let digits: String = dot.chars().take_while(|c| c.is_ascii_digit()).collect();
            let ms_str = if digits.len() >= 3 {
                digits[..3].to_string()
            } else {
                format!("{:<03}", digits)
            };
            ms_str.parse::<u128>().ok()
        })
        .unwrap_or(0);
    let days = civil_to_days(year, month, day)?;
    let seconds = days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    let ms = (seconds as u128) * 1000 + fractional;
    Some(ms)
}

/// Civil-from-days (Hinnant) but inverted: days_from_civil.
fn civil_to_days(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ts_v3_header() {
        let line = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-05-14T03:42:11.123Z","cwd":"/tmp/work","parentSession":null}"#;
        let header = parse_ts_header(line).expect("header");
        assert_eq!(header.version, 3);
        assert_eq!(header.id, "sess-1");
        assert_eq!(header.cwd.as_deref(), Some("/tmp/work"));
        assert!(header.created_ms > 1_700_000_000_000);
    }

    #[test]
    fn rejects_native_header_shape() {
        let line =
            r#"{"type":"session","version":2,"id":"x","created_ms":1700000000000,"cwd":"/tmp"}"#;
        assert!(parse_ts_header(line).is_none());
    }

    #[test]
    fn parses_message_entry_with_text_content() {
        let line = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-14T03:42:11.123Z","message":{"role":"user","content":"你好"}}"#;
        let message = parse_ts_entry(line).expect("entry");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content, "你好");
    }

    #[test]
    fn flattens_content_array_into_text() {
        let line = r#"{"type":"message","id":"m2","parentId":null,"timestamp":"2026-05-14T03:42:11Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi! "},{"type":"tool_use","id":"call_1","name":"ls","input":{"path":"."}}]}}"#;
        let message = parse_ts_entry(line).expect("entry");
        assert_eq!(message.role, Role::Assistant);
        assert!(message.content.starts_with("Hi! "));
        assert!(message.content.contains("[tool_use name=ls"));
    }

    #[test]
    fn skips_unknown_entry_types() {
        let line = r#"{"type":"compaction","id":"c1","parentId":null,"timestamp":"2026-05-14T03:42:11Z","summary":"…","firstKeptEntryId":"m5","tokensBefore":1234}"#;
        assert!(parse_ts_entry(line).is_none());
    }

    #[test]
    fn parse_rfc3339_round_trips_known_epoch() {
        // 2020-01-01T00:00:00Z = 1577836800000 ms
        assert_eq!(
            parse_rfc3339_ms("2020-01-01T00:00:00Z").unwrap(),
            1_577_836_800_000
        );
    }
}
