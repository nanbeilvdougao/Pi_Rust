use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use pi_core::{escape_json_string, Message, PiError, PiErrorKind, PiResult, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
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
    format!(
        "{{\"role\":\"{}\",\"timestamp_ms\":{},\"content\":\"{}\"}}",
        message.role.as_str(),
        message.timestamp_ms,
        escape_json_string(&message.content)
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

    Some(Message::new(role, unescape_json_string(&content)))
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
