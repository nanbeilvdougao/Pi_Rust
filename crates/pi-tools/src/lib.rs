//! Built-in tools for the Pi agent.
//!
//! Tools mirror the TS pi `coding-agent/src/core/tools/*` surface:
//!
//! | Tool   | Purpose                                                   | Mutates |
//! | ------ | --------------------------------------------------------- | ------- |
//! | read   | Read a file or a line range                               | no      |
//! | write  | Overwrite or create a file                                | yes     |
//! | edit   | Replace a unique string in a file with a diff preview     | yes     |
//! | bash   | Execute a shell command with timeout and working dir      | yes     |
//! | search | Plain text search (legacy alias for grep)                 | no      |
//! | grep   | Regex search with line numbers                            | no      |
//! | find   | Glob-based file find                                      | no      |
//! | ls     | List directory entries with file/dir markers              | no      |
//! | epkg   | Plan epkg/openEuler operations (no system execution)      | yes     |
//!
//! Inputs accept BOTH a structured JSON object (used by providers via the
//! `parameters` schema) AND a backward-compatible plain-string form (used by
//! `/tool` prompts in the CLI). This is how we keep the same code path for
//! human-typed shortcuts and provider-driven function calls.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::Deserialize;
use serde_json::{json, Value};

pub mod diff;
pub mod truncate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub name: String,
    pub output: String,
}

pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput>;
}

/// Parsed tool input. Tools should access fields via the helpers (`require_str`
/// etc.) rather than touching the raw value so legacy string and structured
/// JSON callers always see the same shape.
#[derive(Debug, Clone)]
pub struct ToolInput {
    pub raw: String,
    pub value: Value,
}

impl ToolInput {
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim_start();
        let value = if trimmed.starts_with('{') {
            serde_json::from_str(trimmed).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        Self {
            raw: raw.to_string(),
            value,
        }
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.value.get(key).and_then(|v| v.as_str())
    }

    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.value.get(key).and_then(|v| v.as_u64())
    }

    pub fn bool_field(&self, key: &str) -> Option<bool> {
        self.value.get(key).and_then(|v| v.as_bool())
    }
}

#[derive(Default)]
pub struct ToolRuntime {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRuntime {
    pub fn builtin() -> Self {
        let mut runtime = Self::default();
        for tool in builtin_tools() {
            runtime.register(tool);
        }
        runtime
    }

    pub fn builtin_with_names(names: &[String]) -> PiResult<Self> {
        let available = builtin_tools()
            .into_iter()
            .map(|tool| (tool.schema().name.clone(), tool))
            .collect::<BTreeMap<_, _>>();
        let mut runtime = Self::default();
        let mut unknown = Vec::new();
        let mut taken = available;
        for name in names {
            match taken.remove(name) {
                Some(tool) => runtime.register(tool),
                None => unknown.push(name.clone()),
            }
        }
        if !unknown.is_empty() {
            return Err(PiError::new(
                PiErrorKind::Tool,
                format!("未知工具：{}", unknown.join(", ")),
            ));
        }
        Ok(runtime)
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.schema().name.clone(), tool);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub fn run(&self, call: ToolCall, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Tool,
                format!("未知工具：{}。请检查工具名称。", call.name),
            )
        })?;
        let input = ToolInput::parse(&call.input);
        tool.run(&input, permissions)
    }
}

fn builtin_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(BashTool),
        Box::new(SearchTool),
        Box::new(GrepTool),
        Box::new(FindTool),
        Box::new(LsTool),
        Box::new(EpkgTool),
    ]
}

#[derive(Debug, Default, Clone, Copy)]
struct ReadTool;
#[derive(Debug, Default, Clone, Copy)]
struct WriteTool;
#[derive(Debug, Default, Clone, Copy)]
struct EditTool;
#[derive(Debug, Default, Clone, Copy)]
struct BashTool;
#[derive(Debug, Default, Clone, Copy)]
struct EpkgTool;
#[derive(Debug, Default, Clone, Copy)]
struct SearchTool;
#[derive(Debug, Default, Clone, Copy)]
struct GrepTool;
#[derive(Debug, Default, Clone, Copy)]
struct FindTool;
#[derive(Debug, Default, Clone, Copy)]
struct LsTool;

// ===== Read =================================================================

#[derive(Debug, Deserialize, Default)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    line_numbers: Option<bool>,
}

impl Tool for ReadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read".to_string(),
            description: "读取文件内容，可选行号、偏移和限制".to_string(),
            input_shape: "path".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "要读取的文件路径"},
                    "offset": {"type": "integer", "minimum": 1, "description": "起始行号（1 起）"},
                    "limit": {"type": "integer", "minimum": 1, "description": "读取的最大行数"},
                    "line_numbers": {"type": "boolean", "description": "是否在输出中追加行号"}
                },
                "required": ["path"],
                "additionalProperties": false
            })),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: ReadInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            ReadInput {
                path: input.raw.trim().to_string(),
                offset: None,
                limit: None,
                line_numbers: Some(true),
            }
        };

        permissions.require(request(Capability::ReadFile, &parsed.path, "读取文件"))?;
        let content = fs::read_to_string(&parsed.path).map_err(|err| {
            PiError::new(
                if err.kind() == std::io::ErrorKind::NotFound {
                    PiErrorKind::NotFound
                } else {
                    PiErrorKind::Io
                },
                format!("读取 {} 失败：{err}", parsed.path),
            )
        })?;

        let offset = parsed.offset.unwrap_or(1).max(1) as usize;
        let limit = parsed.limit.unwrap_or(u32::MAX) as usize;
        let line_numbers = parsed.line_numbers.unwrap_or(true);

        let mut out = String::new();
        for (idx, line) in content
            .lines()
            .enumerate()
            .skip(offset.saturating_sub(1))
            .take(limit)
        {
            if line_numbers {
                out.push_str(&format!("{:>5}\t{}\n", idx + 1, line));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        if !content.ends_with('\n') && !out.is_empty() {
            // Trim the trailing newline we added since the source didn't end with one.
            if out.ends_with('\n') {
                out.pop();
            }
        }
        Ok(output_for("read", out))
    }
}

// ===== Write ================================================================

#[derive(Debug, Deserialize, Default)]
struct WriteInput {
    path: String,
    content: String,
    #[serde(default)]
    create_dirs: Option<bool>,
}

impl Tool for WriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write".to_string(),
            description: "写入或覆盖文件内容".to_string(),
            input_shape: "path\\ncontent".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "create_dirs": {"type": "boolean", "description": "缺少父目录时自动创建"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: WriteInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            let (path, content) = split_once_line(&input.raw)?;
            WriteInput {
                path: path.to_string(),
                content: content.to_string(),
                create_dirs: Some(false),
            }
        };

        permissions.require(request(Capability::WriteFile, &parsed.path, "写入文件"))?;
        if parsed.create_dirs.unwrap_or(false) {
            if let Some(parent) = Path::new(&parsed.path).parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    fs::create_dir_all(parent)?;
                }
            }
        }
        fs::write(&parsed.path, &parsed.content)?;
        Ok(output_for(
            "write",
            format!("已写入 {}（{} 字节）", parsed.path, parsed.content.len()),
        ))
    }
}

// ===== Edit =================================================================

#[derive(Debug, Deserialize, Default)]
struct EditInput {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    expect_unique: Option<bool>,
    #[serde(default)]
    replace_all: Option<bool>,
}

impl Tool for EditTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit".to_string(),
            description: "替换文件中的文本，默认要求唯一匹配，附带 diff 预览".to_string(),
            input_shape: "path\\nold\\n---\\nnew".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "expect_unique": {"type": "boolean", "default": true},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: EditInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            let (path, rest) = split_once_line(&input.raw)?;
            let (old, new) = rest.split_once("\n---\n").ok_or_else(|| {
                PiError::new(
                    PiErrorKind::InvalidInput,
                    "edit 工具输入必须包含 `\\n---\\n` 分隔符",
                )
            })?;
            EditInput {
                path: path.to_string(),
                old: old.to_string(),
                new: new.to_string(),
                expect_unique: Some(true),
                replace_all: Some(false),
            }
        };

        permissions.require(request(Capability::WriteFile, &parsed.path, "编辑文件"))?;
        let current = fs::read_to_string(&parsed.path).map_err(|err| {
            PiError::new(
                if err.kind() == std::io::ErrorKind::NotFound {
                    PiErrorKind::NotFound
                } else {
                    PiErrorKind::Io
                },
                format!("读取 {} 失败：{err}", parsed.path),
            )
        })?;
        if parsed.old == parsed.new {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "edit 工具 old 与 new 相同",
            ));
        }
        let occurrences = current.matches(&parsed.old).count();
        if occurrences == 0 {
            return Err(PiError::new(PiErrorKind::Tool, "edit 未找到要替换的文本"));
        }
        let replace_all = parsed.replace_all.unwrap_or(false);
        let expect_unique = parsed.expect_unique.unwrap_or(true);
        if !replace_all && expect_unique && occurrences > 1 {
            return Err(PiError::new(
                PiErrorKind::Tool,
                format!(
                    "edit 找到 {occurrences} 次 old 匹配，请改用 replace_all 或加上更多上下文以确保唯一"
                ),
            ));
        }

        let edited = if replace_all {
            current.replace(&parsed.old, &parsed.new)
        } else {
            current.replacen(&parsed.old, &parsed.new, 1)
        };

        let preview = diff::unified(&current, &edited, &parsed.path);
        fs::write(&parsed.path, &edited)?;
        Ok(output_for(
            "edit",
            format!(
                "已编辑 {} (replaced {} 处)\n{preview}",
                parsed.path,
                if replace_all { occurrences } else { 1 }
            ),
        ))
    }
}

// ===== Bash =================================================================

#[derive(Debug, Deserialize, Default)]
struct BashInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Tool for BashTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".to_string(),
            description: "执行 shell 命令，支持工作目录和超时".to_string(),
            input_shape: "command".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600000, "default": 120000}
                },
                "required": ["command"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: BashInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            BashInput {
                command: input.raw.clone(),
                cwd: None,
                timeout_ms: None,
            }
        };

        permissions.require(request(
            Capability::ExecuteCommand,
            &parsed.command,
            "执行命令",
        ))?;

        let mut command = Command::new("sh");
        command.arg("-c").arg(&parsed.command);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(cwd) = &parsed.cwd {
            command.current_dir(cwd);
        }
        let timeout = Duration::from_millis(parsed.timeout_ms.unwrap_or(120_000));

        let mut child = command.spawn()?;
        let start = Instant::now();
        loop {
            match child.try_wait()? {
                Some(_status) => break,
                None => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        return Err(PiError::new(
                            PiErrorKind::Tool,
                            format!("命令超时 {}ms", timeout.as_millis()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let output = child.wait_with_output()?;

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            text.push_str("--- stderr ---\n");
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        text.push_str(&format!(
            "\n--- exit {} ---",
            output.status.code().unwrap_or(-1)
        ));
        Ok(output_for(
            "bash",
            truncate::truncate(&text, truncate::TruncationPolicy::default()),
        ))
    }
}

// ===== EPKG =================================================================

impl Tool for EpkgTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "epkg".to_string(),
            description: "规划 epkg/openEuler 包管理操作（不直接修改系统）".to_string(),
            input_shape: "subcommand".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "subcommand": {"type": "string", "description": "epkg 子命令及参数"}
                },
                "required": ["subcommand"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let subcommand = input
            .str_field("subcommand")
            .map(|s| s.to_string())
            .unwrap_or_else(|| input.raw.clone());
        permissions.require(request(
            Capability::ExecuteCommand,
            &subcommand,
            "规划 epkg/openEuler 包管理操作",
        ))?;
        Ok(output_for(
            "epkg",
            format!("epkg/openEuler 操作仅停留在计划阶段，未执行。建议命令：epkg {subcommand}"),
        ))
    }
}

// ===== Search / Grep ========================================================

#[derive(Debug, Deserialize, Default)]
struct GrepInput {
    pattern: String,
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    max_results: Option<u32>,
}

fn default_search_path() -> String {
    ".".to_string()
}

impl Tool for SearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search".to_string(),
            description: "纯文本递归搜索（grep 的简化别名）".to_string(),
            input_shape: "path\\npattern".to_string(),
            parameters: Some(grep_parameters()),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: GrepInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            let (path, pattern) = split_once_line(&input.raw)?;
            GrepInput {
                pattern: pattern.to_string(),
                path: path.to_string(),
                case_sensitive: Some(true),
                max_results: None,
            }
        };
        permissions.require(request(Capability::ReadFile, &parsed.path, "搜索文件"))?;
        let matches = plain_search(
            Path::new(&parsed.path),
            &parsed.pattern,
            parsed.case_sensitive.unwrap_or(true),
            parsed.max_results.unwrap_or(200),
        )?;
        Ok(output_for("search", matches.join("\n")))
    }
}

impl Tool for GrepTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".to_string(),
            description: "正则递归搜索，输出 file:line:text".to_string(),
            input_shape: "pattern".to_string(),
            parameters: Some(grep_parameters()),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: GrepInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            GrepInput {
                pattern: input.raw.clone(),
                path: ".".to_string(),
                case_sensitive: Some(true),
                max_results: None,
            }
        };
        permissions.require(request(Capability::ReadFile, &parsed.path, "搜索文件"))?;
        let regex = regex::RegexBuilder::new(&parsed.pattern)
            .case_insensitive(!parsed.case_sensitive.unwrap_or(true))
            .build()
            .map_err(|err| PiError::new(PiErrorKind::InvalidInput, format!("无效正则：{err}")))?;
        let matches = regex_search(
            Path::new(&parsed.path),
            &regex,
            parsed.max_results.unwrap_or(500),
        )?;
        Ok(output_for("grep", matches.join("\n")))
    }
}

fn grep_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string"},
            "path": {"type": "string", "default": "."},
            "case_sensitive": {"type": "boolean", "default": true},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 5000}
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

fn plain_search(
    path: &Path,
    pattern: &str,
    case_sensitive: bool,
    max_results: u32,
) -> PiResult<Vec<String>> {
    let mut matches: Vec<String> = Vec::new();
    let lowered = pattern.to_ascii_lowercase();
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|err| {
            PiError::new(PiErrorKind::Io, format!("遍历 {}: {err}", path.display()))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let content = match fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for (idx, line) in content.lines().enumerate() {
            let hit = if case_sensitive {
                line.contains(pattern)
            } else {
                line.to_ascii_lowercase().contains(&lowered)
            };
            if hit {
                matches.push(format!("{}:{}:{}", entry.path().display(), idx + 1, line));
                if matches.len() as u32 >= max_results {
                    return Ok(matches);
                }
            }
        }
    }
    Ok(matches)
}

fn regex_search(path: &Path, regex: &regex::Regex, max_results: u32) -> PiResult<Vec<String>> {
    let mut matches: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(|err| {
            PiError::new(PiErrorKind::Io, format!("遍历 {}: {err}", path.display()))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let content = match fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue,
        };
        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(format!("{}:{}:{}", entry.path().display(), idx + 1, line));
                if matches.len() as u32 >= max_results {
                    return Ok(matches);
                }
            }
        }
    }
    Ok(matches)
}

// ===== Find =================================================================

#[derive(Debug, Deserialize, Default)]
struct FindInput {
    glob: String,
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default)]
    max_results: Option<u32>,
}

impl Tool for FindTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "find".to_string(),
            description: "按 glob 递归查找文件".to_string(),
            input_shape: "glob".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "glob": {"type": "string"},
                    "path": {"type": "string", "default": "."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 10000}
                },
                "required": ["glob"],
                "additionalProperties": false
            })),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: FindInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            FindInput {
                glob: input.raw.clone(),
                path: ".".to_string(),
                max_results: None,
            }
        };
        permissions.require(request(Capability::ReadFile, &parsed.path, "查找文件"))?;
        let glob = globset::Glob::new(&parsed.glob)
            .map_err(|err| PiError::new(PiErrorKind::InvalidInput, format!("无效 glob：{err}")))?;
        let matcher = glob.compile_matcher();
        let mut out: Vec<String> = Vec::new();
        let cap = parsed.max_results.unwrap_or(1000) as usize;
        for entry in walkdir::WalkDir::new(&parsed.path).follow_links(false) {
            let entry = entry.map_err(|err| {
                PiError::new(PiErrorKind::Io, format!("遍历 {}: {err}", parsed.path))
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let candidate = path.strip_prefix(&parsed.path).unwrap_or(path);
            if matcher.is_match(candidate) || matcher.is_match(path) {
                out.push(path.display().to_string());
                if out.len() >= cap {
                    break;
                }
            }
        }
        Ok(output_for("find", out.join("\n")))
    }
}

// ===== Ls ===================================================================

#[derive(Debug, Deserialize, Default)]
struct LsInput {
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default)]
    show_hidden: Option<bool>,
}

impl Tool for LsTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ls".to_string(),
            description: "列出目录内容".to_string(),
            input_shape: "path".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "default": "."},
                    "show_hidden": {"type": "boolean", "default": false}
                },
                "additionalProperties": false
            })),
            mutates: false,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: LsInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            LsInput {
                path: if input.raw.trim().is_empty() {
                    ".".to_string()
                } else {
                    input.raw.trim().to_string()
                },
                show_hidden: Some(false),
            }
        };
        permissions.require(request(Capability::ReadFile, &parsed.path, "列出目录"))?;
        let show_hidden = parsed.show_hidden.unwrap_or(false);
        let mut entries: Vec<String> = Vec::new();
        for entry in fs::read_dir(&parsed.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let suffix = if path.is_dir() { "/" } else { "" };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();
        Ok(output_for("ls", entries.join("\n")))
    }
}

fn request(capability: Capability, target: &str, reason: &str) -> PermissionRequest {
    PermissionRequest {
        capability,
        target: target.to_string(),
        reason: reason.to_string(),
    }
}

fn output_for(name: &str, output: String) -> ToolOutput {
    ToolOutput {
        name: name.to_string(),
        output,
    }
}

fn split_once_line(input: &str) -> PiResult<(&str, &str)> {
    input.split_once('\n').ok_or_else(|| {
        PiError::new(
            PiErrorKind::InvalidInput,
            "工具输入格式错误：缺少第一行参数",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_permissions::PermissionMode;
    use std::fs;
    use tempfile::tempdir;

    fn runtime() -> ToolRuntime {
        ToolRuntime::builtin()
    }

    fn permissions() -> PermissionEngine {
        PermissionEngine::new(PermissionMode::TrustedWorkspace)
    }

    #[test]
    fn read_handles_line_numbers_and_range() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let runtime = runtime();
        let mut perms = permissions();
        let output = runtime
            .run(
                ToolCall {
                    name: "read".to_string(),
                    input: serde_json::to_string(&json!({"path": file, "offset": 2, "limit": 1}))
                        .unwrap(),
                },
                &mut perms,
            )
            .unwrap();
        assert!(output.output.contains("2\tbeta"));
        assert!(!output.output.contains("alpha"));
    }

    #[test]
    fn edit_replaces_unique_string_and_returns_diff() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("note.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        let runtime = runtime();
        let mut perms = permissions();
        let output = runtime
            .run(
                ToolCall {
                    name: "edit".to_string(),
                    input: serde_json::to_string(&json!({
                        "path": file,
                        "old": "alpha",
                        "new": "ALPHA",
                    }))
                    .unwrap(),
                },
                &mut perms,
            )
            .unwrap();
        assert!(output.output.contains("已编辑"));
        assert!(output.output.contains("-alpha"));
        assert!(output.output.contains("+ALPHA"));
        let new_content = fs::read_to_string(&file).unwrap();
        assert!(new_content.contains("ALPHA"));
    }

    #[test]
    fn edit_refuses_ambiguous_match() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("dup.txt");
        fs::write(&file, "x\nx\n").unwrap();
        let runtime = runtime();
        let mut perms = permissions();
        let result = runtime.run(
            ToolCall {
                name: "edit".to_string(),
                input: serde_json::to_string(&json!({"path": file, "old": "x", "new": "y"}))
                    .unwrap(),
            },
            &mut perms,
        );
        assert!(result.is_err());
    }

    #[test]
    fn grep_finds_lines_with_regex() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello\nworld\n").unwrap();
        let runtime = runtime();
        let mut perms = permissions();
        let out = runtime
            .run(
                ToolCall {
                    name: "grep".to_string(),
                    input: serde_json::to_string(&json!({
                        "pattern": "^h",
                        "path": dir.path(),
                    }))
                    .unwrap(),
                },
                &mut perms,
            )
            .unwrap();
        assert!(out.output.contains("hello"));
        assert!(!out.output.contains("world"));
    }

    #[test]
    fn find_supports_glob_filter() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.txt");
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let runtime = runtime();
        let mut perms = permissions();
        let out = runtime
            .run(
                ToolCall {
                    name: "find".to_string(),
                    input: serde_json::to_string(&json!({"glob": "*.rs", "path": dir.path()}))
                        .unwrap(),
                },
                &mut perms,
            )
            .unwrap();
        assert!(out.output.contains("a.rs"));
        assert!(!out.output.contains("b.txt"));
    }

    #[test]
    fn write_creates_parents_when_asked() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested/dir/file.txt");
        let runtime = runtime();
        let mut perms = permissions();
        let output = runtime
            .run(
                ToolCall {
                    name: "write".to_string(),
                    input: serde_json::to_string(&json!({
                        "path": target,
                        "content": "hi",
                        "create_dirs": true,
                    }))
                    .unwrap(),
                },
                &mut perms,
            )
            .unwrap();
        assert!(output.output.contains("已写入"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "hi");
    }
}
