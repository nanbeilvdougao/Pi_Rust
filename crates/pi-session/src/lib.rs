//! JSONL-based append-only session storage.
//!
//! Sessions live as `<root>/<id>.jsonl`. Each line is a JSON-encoded
//! [`Message`]. The format is forward-compatible with the legacy hand-rolled
//! emitter and intentionally simple so external tools can grep / replay.
//!
//! Design notes:
//! - Append is the hot path; we open with `O_APPEND` to avoid races between
//!   concurrent agents.
//! - Loading is tolerant of partial lines (a crashed write produces a single
//!   incomplete trailing line which we skip).
//! - Session IDs are sanitized: only `[A-Za-z0-9._-]` survives, anything else
//!   becomes `_`. This is the defense in depth against `../` traversal on top
//!   of `ensure_safe_session_path`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pi_core::{Message, PiError, PiErrorKind, PiResult};
use serde::{Deserialize, Serialize};

#[cfg(feature = "sqlite-index")]
pub mod sqlite_index;
#[cfg(feature = "sqlite-index")]
pub use sqlite_index::SqliteIndex;

pub mod export_html;
pub mod interop;

/// On-disk JSONL session schema version. Incremented when the header layout
/// or message envelope changes; loaders tolerate older versions.
pub const SESSION_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionHeader {
    #[serde(rename = "type", default = "default_header_type")]
    pub kind: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub created_ms: u128,
    /// Working directory at session creation. The agent can `chdir` here on
    /// resume so file paths in the transcript stay valid.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

fn default_header_type() -> String {
    "session".to_string()
}

fn default_version() -> u32 {
    SESSION_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<SessionHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub message_count: usize,
    pub updated_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_excerpt: Option<String>,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            messages: Vec::new(),
            title: None,
            provider: None,
            model: None,
            header: None,
        }
    }

    pub fn cwd(&self) -> Option<&str> {
        self.header.as_ref().and_then(|h| h.cwd.as_deref())
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn last_user_excerpt(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, pi_core::Role::User))
            .map(|m| truncate_excerpt(&m.content, 80))
    }
}

fn truncate_excerpt(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

pub trait SessionStore {
    fn load(&self, id: &str) -> PiResult<Session>;
    fn append(&self, id: &str, message: &Message) -> PiResult<()>;
}

/// Non-persistent session store. Used for subagents (`task` tool) so a child
/// agent run does not pollute the parent's `.jsonl` files. Thread-safe; the
/// inner map is wrapped in `Mutex` so multiple subagents can share a store.
#[derive(Debug, Default)]
pub struct InMemorySessionStore {
    inner: std::sync::Mutex<std::collections::BTreeMap<String, Vec<Message>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }
}

impl SessionStore for InMemorySessionStore {
    fn load(&self, id: &str) -> PiResult<Session> {
        let guard = self
            .inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Session, format!("锁失败：{err}")))?;
        let messages = guard.get(id).cloned().unwrap_or_default();
        Ok(Session {
            id: id.to_string(),
            messages,
            title: None,
            provider: None,
            model: None,
            header: None,
        })
    }

    fn append(&self, id: &str, message: &Message) -> PiResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Session, format!("锁失败：{err}")))?;
        guard.entry(id.to_string()).or_default().push(message.clone());
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct JsonlSessionStore {
    root: PathBuf,
}

impl JsonlSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.jsonl", sanitize_id(id)))
    }

    pub fn delete(&self, id: &str) -> PiResult<bool> {
        let path = self.session_path(id);
        ensure_safe_session_path(&self.root, &path)?;
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(path)?;
        Ok(true)
    }

    pub fn rename(&self, from: &str, to: &str) -> PiResult<()> {
        let from_path = self.session_path(from);
        let to_path = self.session_path(to);
        ensure_safe_session_path(&self.root, &from_path)?;
        ensure_safe_session_path(&self.root, &to_path)?;
        if !from_path.exists() {
            return Err(PiError::new(
                PiErrorKind::NotFound,
                format!("会话不存在：{from}"),
            ));
        }
        if to_path.exists() {
            return Err(PiError::new(
                PiErrorKind::Session,
                format!("目标会话已存在：{to}"),
            ));
        }
        fs::rename(from_path, to_path)?;
        Ok(())
    }

    pub fn export_markdown(&self, id: &str) -> PiResult<String> {
        let session = self.load(id)?;
        if session.messages.is_empty() {
            return Err(PiError::new(
                PiErrorKind::NotFound,
                format!("会话为空或不存在：{id}"),
            ));
        }

        let mut out = format!("# Pi Session: {id}\n\n");
        for message in session.messages {
            out.push_str("## ");
            out.push_str(message.role.as_str());
            if let Some(tool_call_id) = message.tool_call_id {
                out.push(' ');
                out.push_str(&tool_call_id);
            }
            out.push_str("\n\n");
            out.push_str(&message.content);
            out.push_str("\n\n");
        }
        Ok(out)
    }

    pub fn export_json(&self, id: &str) -> PiResult<String> {
        let session = self.load(id)?;
        Ok(serde_json::to_string_pretty(&session)?)
    }

    pub fn export_html(&self, id: &str) -> PiResult<String> {
        let session = self.load(id)?;
        Ok(export_html::render(&session))
    }

    pub fn list(&self) -> PiResult<Vec<SessionSummary>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };

            let metadata = entry.metadata()?;
            let updated_ms = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or(0);

            let summary_session = self.load(stem).unwrap_or_else(|_| Session::new(stem));

            sessions.push(SessionSummary {
                id: stem.to_string(),
                message_count: summary_session.messages.len(),
                updated_ms,
                last_user_excerpt: summary_session.last_user_excerpt(),
            });
        }

        sessions.sort_by(|left, right| {
            right
                .updated_ms
                .cmp(&left.updated_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }
}

impl SessionStore for JsonlSessionStore {
    fn load(&self, id: &str) -> PiResult<Session> {
        let path = self.session_path(id);
        if !path.exists() {
            return Ok(Session::new(id));
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut session = Session::new(id);

        for (line_idx, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            // First non-empty line: try parsing as a header. Accept both our
            // native shape and the TS-pi v3 header so a TS-written .jsonl
            // loads without conversion.
            if line_idx == 0 || session.header.is_none() {
                if let Ok(header) = serde_json::from_str::<SessionHeader>(&line) {
                    if header.kind == "session" {
                        session.header = Some(header);
                        continue;
                    }
                }
                if let Some(header) = interop::parse_ts_header(&line) {
                    session.header = Some(header);
                    continue;
                }
            }
            match serde_json::from_str::<Message>(&line) {
                Ok(message) => session.push(message),
                Err(_) => {
                    if let Some(message) = interop::parse_ts_entry(&line) {
                        session.push(message);
                    } else if let Some(message) = legacy_parse_message(&line) {
                        session.push(message);
                    }
                }
            }
        }

        Ok(session)
    }

    fn append(&self, id: &str, message: &Message) -> PiResult<()> {
        fs::create_dir_all(&self.root)?;
        let path = self.session_path(id);
        ensure_safe_session_path(&self.root, &path)?;
        let header_needed = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if header_needed {
            let header = SessionHeader {
                kind: "session".to_string(),
                version: SESSION_VERSION,
                id: id.to_string(),
                created_ms: pi_core::now_ms(),
                cwd: std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string()),
                parent_session: None,
                title: None,
                provider: None,
                model: None,
            };
            let header_line = serde_json::to_string(&header)?;
            writeln!(file, "{header_line}")?;
        }
        let line = serde_json::to_string(message)?;
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }
}

pub fn format_jsonl_message(message: &Message) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| String::new())
}

pub fn parse_jsonl_message(line: &str) -> Option<Message> {
    serde_json::from_str::<Message>(line)
        .ok()
        .or_else(|| legacy_parse_message(line))
}

fn legacy_parse_message(line: &str) -> Option<Message> {
    use pi_core::Role;
    let role = extract_legacy_json_field(line, "role")?;
    let content = extract_legacy_json_field(line, "content")?;
    let role = match role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => return None,
    };
    let mut message = Message::new(role, legacy_unescape(&content));
    if let Some(id) = extract_legacy_json_field(line, "tool_call_id") {
        message.tool_call_id = Some(legacy_unescape(&id));
    }
    Some(message)
}

fn extract_legacy_json_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let mut out = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn legacy_unescape(input: &str) -> String {
    input
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn sanitize_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

fn ensure_safe_session_path(root: &Path, path: &Path) -> PiResult<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(PiError::new(
            PiErrorKind::Session,
            "会话路径不在允许的会话目录内",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, Role};
    use tempfile::tempdir;

    #[test]
    fn append_then_load_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(dir.path());
        let msg = Message::new(Role::User, "你好");
        store.append("alpha", &msg).expect("append");
        let session = store.load("alpha").expect("load");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "你好");
    }

    #[test]
    fn legacy_lines_are_tolerated() {
        let line = r#"{"role":"assistant","timestamp_ms":1,"content":"hi"}"#;
        let parsed = parse_jsonl_message(line).expect("parse");
        assert_eq!(parsed.role, Role::Assistant);
        assert_eq!(parsed.content, "hi");
    }

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize_id("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_id(""), "default");
        assert_eq!(sanitize_id("ok-id.1"), "ok-id.1");
    }

    #[test]
    fn list_sorts_by_updated_descending() {
        let dir = tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(dir.path());
        store
            .append("first", &Message::new(Role::User, "1"))
            .expect("append");
        std::thread::sleep(std::time::Duration::from_millis(10));
        store
            .append("second", &Message::new(Role::User, "2"))
            .expect("append");
        let summaries = store.list().expect("list");
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "second");
    }

    #[test]
    fn loads_ts_pi_v3_session_jsonl() {
        let dir = tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(dir.path());
        let ts_file = dir.path().join("ts-session.jsonl");
        std::fs::write(
            &ts_file,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"ts\",\"timestamp\":\"2026-05-14T03:42:11.123Z\",\"cwd\":\"/tmp/ws\",\"parentSession\":null}\n",
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-05-14T03:42:12Z\",\"message\":{\"role\":\"user\",\"content\":\"你好\"}}\n",
                "{\"type\":\"compaction\",\"id\":\"c1\",\"parentId\":null,\"timestamp\":\"2026-05-14T03:42:13Z\",\"summary\":\"…\",\"firstKeptEntryId\":\"m1\",\"tokensBefore\":1234}\n",
                "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":null,\"timestamp\":\"2026-05-14T03:42:14Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"好的\"}]}}\n",
            ),
        )
        .unwrap();
        let session = store.load("ts-session").expect("load");
        let header = session.header.as_ref().expect("header");
        assert_eq!(header.version, 3);
        assert_eq!(header.cwd.as_deref(), Some("/tmp/ws"));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "你好");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "好的");
    }
}
