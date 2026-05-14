//! `todo` tool — manages a session-scoped task list the agent can use to
//! plan multi-step work and keep the user looped in.
//!
//! Storage: `<cwd>/.pi/todos/<session_id>.json`. The session id is read from
//! `PI_SESSION_ID` (set by the agent runtime before each turn). When the
//! variable is missing, we fall back to `default` so the tool is still
//! usable in one-shot CLI flows.
//!
//! Actions:
//! - `add` — append a new item, returns its assigned id.
//! - `update` — set status (`pending` / `in_progress` / `completed`) by id;
//!   one update per call, but accepts an array of ids to bulk-update.
//! - `list` — read all items.
//! - `clear` — delete the entire list.
//!
//! Schema is intentionally minimal so the model picks it up quickly. The
//! TUI renders the list via `pi-tui::todo_panel` (see #91).
//!
//! Parity target: `packages/agent/src/tools/todo.ts`.

use std::env;
use std::fs;
use std::path::PathBuf;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Tool, ToolInput, ToolOutput};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct TodoTool;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum TodoAction {
    Add { text: String },
    Update { id: u32, status: TodoStatus },
    List,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn glyph(self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[…]",
            TodoStatus::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u32,
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoList {
    #[serde(default)]
    pub items: Vec<TodoItem>,
    #[serde(default)]
    pub next_id: u32,
}

impl TodoList {
    pub fn add(&mut self, text: String) -> u32 {
        let id = self.next_id + 1;
        self.next_id = id;
        self.items.push(TodoItem {
            id,
            text,
            status: TodoStatus::Pending,
        });
        id
    }

    pub fn update(&mut self, id: u32, status: TodoStatus) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.status = status;
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 0;
    }

    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "(待办为空)".to_string();
        }
        let mut out = String::new();
        for item in &self.items {
            out.push_str(item.status.glyph());
            out.push(' ');
            out.push_str(&format!("#{} ", item.id));
            out.push_str(&item.text);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

impl Tool for TodoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "todo".to_string(),
            description: "管理会话级 todo 列表：add/update/list/clear".to_string(),
            input_shape: "json".to_string(),
            parameters: Some(json!({
                "type": "object",
                "oneOf": [
                    {"properties": {"action": {"const": "add"}, "text": {"type": "string"}}, "required": ["action", "text"]},
                    {"properties": {"action": {"const": "update"}, "id": {"type": "integer"}, "status": {"enum": ["pending", "in_progress", "completed"]}}, "required": ["action", "id", "status"]},
                    {"properties": {"action": {"const": "list"}}, "required": ["action"]},
                    {"properties": {"action": {"const": "clear"}}, "required": ["action"]}
                ]
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let action: TodoAction = serde_json::from_value(input.value.clone()).map_err(|err| {
            PiError::new(
                PiErrorKind::InvalidInput,
                format!("todo 输入不合法：{err}; raw={}", input.raw),
            )
        })?;
        let path = todo_path();
        // Gate on WriteFile only for mutations.
        if !matches!(action, TodoAction::List) {
            permissions.require(PermissionRequest {
                capability: Capability::WriteFile,
                target: path.display().to_string(),
                reason: "更新会话 todo 列表".to_string(),
            })?;
        }
        let mut list = load(&path)?;
        let output = match action {
            TodoAction::Add { text } => {
                let id = list.add(text.clone());
                save(&path, &list)?;
                format!("#{id} 已添加：{text}\n\n{}", list.render())
            }
            TodoAction::Update { id, status } => {
                if !list.update(id, status) {
                    return Err(PiError::new(
                        PiErrorKind::InvalidInput,
                        format!("todo #{id} 不存在"),
                    ));
                }
                save(&path, &list)?;
                format!(
                    "#{id} 已更新为 {}\n\n{}",
                    match status {
                        TodoStatus::Pending => "pending",
                        TodoStatus::InProgress => "in_progress",
                        TodoStatus::Completed => "completed",
                    },
                    list.render()
                )
            }
            TodoAction::List => list.render(),
            TodoAction::Clear => {
                list.clear();
                save(&path, &list)?;
                "(已清空)".to_string()
            }
        };
        Ok(ToolOutput {
            name: "todo".to_string(),
            output,
        })
    }
}

fn session_id() -> String {
    env::var("PI_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn todo_path() -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join(".pi").join("todos").join(format!("{}.json", session_id()))
}

pub fn load(path: &PathBuf) -> PiResult<TodoList> {
    if !path.exists() {
        return Ok(TodoList::default());
    }
    let text = fs::read_to_string(path)
        .map_err(|err| PiError::new(PiErrorKind::Io, format!("读取 todo 文件失败：{err}")))?;
    if text.trim().is_empty() {
        return Ok(TodoList::default());
    }
    serde_json::from_str(&text).map_err(|err| {
        PiError::new(
            PiErrorKind::Io,
            format!("解析 todo 文件失败：{err}; path={}", path.display()),
        )
    })
}

pub fn save(path: &PathBuf, list: &TodoList) -> PiResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PiError::new(PiErrorKind::Io, format!("创建 todo 目录失败：{err}")))?;
    }
    let text = serde_json::to_string_pretty(list)
        .map_err(|err| PiError::new(PiErrorKind::Io, format!("序列化 todo 失败：{err}")))?;
    fs::write(path, text)
        .map_err(|err| PiError::new(PiErrorKind::Io, format!("写入 todo 失败：{err}")))?;
    Ok(())
}

/// Helper for callers that want the current session's todo list without
/// touching the filesystem path directly. Returns an empty list if no file
/// exists yet.
pub fn load_current() -> PiResult<TodoList> {
    load(&todo_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn list_in(dir: &TempDir) -> PathBuf {
        dir.path().join("todos.json")
    }

    #[test]
    fn add_and_update_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = list_in(&dir);
        let mut list = TodoList::default();
        let id = list.add("first".into());
        list.add("second".into());
        assert_eq!(id, 1);
        save(&path, &list).expect("save");
        let mut loaded = load(&path).expect("load");
        assert_eq!(loaded.items.len(), 2);
        assert!(loaded.update(1, TodoStatus::Completed));
        assert!(!loaded.update(99, TodoStatus::Completed));
        save(&path, &loaded).expect("save");
        let again = load(&path).expect("load");
        assert_eq!(again.items[0].status, TodoStatus::Completed);
    }

    #[test]
    fn render_marks_status_with_glyphs() {
        let mut list = TodoList::default();
        list.add("alpha".into());
        list.add("beta".into());
        list.update(2, TodoStatus::InProgress);
        let text = list.render();
        assert!(text.contains("[ ] #1 alpha"));
        assert!(text.contains("[…] #2 beta"));
    }

    #[test]
    fn clear_resets_counter() {
        let mut list = TodoList::default();
        list.add("x".into());
        list.add("y".into());
        list.clear();
        assert!(list.items.is_empty());
        assert_eq!(list.next_id, 0);
        let new_id = list.add("z".into());
        assert_eq!(new_id, 1);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.json");
        let list = load(&path).expect("load");
        assert!(list.items.is_empty());
    }
}
