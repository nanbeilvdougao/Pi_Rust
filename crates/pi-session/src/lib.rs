use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pi_core::{escape_json_string, Message, PiError, PiErrorKind, PiResult, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub message_count: usize,
    pub updated_ms: u128,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            messages: Vec::new(),
        }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }
}

pub trait SessionStore {
    fn load(&self, id: &str) -> PiResult<Session>;
    fn append(&self, id: &str, message: &Message) -> PiResult<()>;
}

#[derive(Debug, Clone)]
pub struct JsonlSessionStore {
    root: PathBuf,
}

impl JsonlSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
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
                PiErrorKind::Session,
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
                PiErrorKind::Session,
                format!("会话为空或不存在：{id}"),
            ));
        }

        let mut out = format!("# Pi Session: {id}\n\n");
        for message in session.messages {
            out.push_str("## ");
            out.push_str(message.role.as_str());
            if let Some(tool_call_id) = message.tool_call_id {
                out.push_str(" ");
                out.push_str(&tool_call_id);
            }
            out.push_str("\n\n");
            out.push_str(&message.content);
            out.push_str("\n\n");
        }
        Ok(out)
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

            sessions.push(SessionSummary {
                id: stem.to_string(),
                message_count: count_lines(&path)?,
                updated_ms,
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

        let file = fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut session = Session::new(id);

        for line in reader.lines() {
            let line = line?;
            if let Some(message) = parse_jsonl_message(&line) {
                session.push(message);
            }
        }

        Ok(session)
    }

    fn append(&self, id: &str, message: &Message) -> PiResult<()> {
        fs::create_dir_all(&self.root)?;
        let path = self.session_path(id);
        ensure_safe_session_path(&self.root, &path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", format_jsonl_message(message))?;
        Ok(())
    }
}

pub fn format_jsonl_message(message: &Message) -> String {
    let tool_call_id = message
        .tool_call_id
        .as_ref()
        .map(|id| format!(",\"tool_call_id\":\"{}\"", escape_json_string(id)))
        .unwrap_or_default();
    format!(
        "{{\"role\":\"{}\",\"timestamp_ms\":{},\"content\":\"{}\"{}}}",
        message.role.as_str(),
        message.timestamp_ms,
        escape_json_string(&message.content),
        tool_call_id
    )
}

pub fn parse_jsonl_message(line: &str) -> Option<Message> {
    let role = extract_json_field(line, "role")?;
    let content = extract_json_field(line, "content")?;
    let role = match role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => return None,
    };

    let mut message = Message::new(role, unescape_json_string(&content));
    message.tool_call_id =
        extract_json_field(line, "tool_call_id").map(|id| unescape_json_string(&id));
    Some(message)
}

fn extract_json_field(line: &str, field: &str) -> Option<String> {
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

fn unescape_json_string(input: &str) -> String {
    input
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
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

fn count_lines(path: &Path) -> PiResult<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut count = 0;
    for line in reader.lines() {
        let _ = line?;
        count += 1;
    }
    Ok(count)
}
