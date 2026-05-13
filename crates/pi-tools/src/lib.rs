use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};

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

pub trait Tool {
    fn schema(&self) -> ToolSchema;
    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput>;
}

#[derive(Default)]
pub struct ToolRuntime {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRuntime {
    pub fn builtin() -> Self {
        let mut runtime = Self::default();
        runtime.register(Box::new(ReadTool));
        runtime.register(Box::new(WriteTool));
        runtime.register(Box::new(EditTool));
        runtime.register(Box::new(BashTool));
        runtime.register(Box::new(EpkgTool));
        runtime.register(Box::new(SearchTool));
        runtime.register(Box::new(LsTool));
        runtime
    }

    pub fn builtin_with_names(names: &[String]) -> PiResult<Self> {
        let mut runtime = Self::default();
        let mut unknown = Vec::new();
        for name in names {
            match name.as_str() {
                "read" => runtime.register(Box::new(ReadTool)),
                "write" => runtime.register(Box::new(WriteTool)),
                "edit" => runtime.register(Box::new(EditTool)),
                "bash" => runtime.register(Box::new(BashTool)),
                "epkg" => runtime.register(Box::new(EpkgTool)),
                "search" => runtime.register(Box::new(SearchTool)),
                "ls" => runtime.register(Box::new(LsTool)),
                other => unknown.push(other.to_string()),
            }
        }

        if unknown.is_empty() {
            Ok(runtime)
        } else {
            Err(PiError::new(
                PiErrorKind::Tool,
                format!("未知工具：{}", unknown.join(", ")),
            ))
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.schema().name.clone(), tool);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub fn run(
        &self,
        call: ToolCall,
        permissions: &mut PermissionEngine,
    ) -> PiResult<ToolOutput> {
        let tool = self.tools.get(&call.name).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Tool,
                format!("未知工具：{}。请检查工具名称。", call.name),
            )
        })?;
        tool.run(&call.input, permissions)
    }
}

struct ReadTool;
struct WriteTool;
struct EditTool;
struct BashTool;
struct EpkgTool;
struct SearchTool;
struct LsTool;

impl Tool for ReadTool {
    fn schema(&self) -> ToolSchema {
        schema("read", "读取文件内容", "path", false)
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        permissions.require(request(Capability::ReadFile, input, "读取文件"))?;
        let output = fs::read_to_string(input)?;
        Ok(output_for("read", output))
    }
}

impl Tool for WriteTool {
    fn schema(&self) -> ToolSchema {
        schema("write", "写入文件内容，输入格式为 path\\ncontent", "path\\ncontent", true)
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let (path, content) = split_once_line(input)?;
        permissions.require(request(Capability::WriteFile, path, "写入文件"))?;
        fs::write(path, content)?;
        Ok(output_for("write", format!("已写入 {path}")))
    }
}

impl Tool for EditTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "edit",
            "替换文件中的文本，输入格式为 path\\nold\\n---\\nnew",
            "path\\nold\\n---\\nnew",
            true,
        )
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let (path, rest) = split_once_line(input)?;
        let (old, new) = rest.split_once("\n---\n").ok_or_else(|| {
            PiError::new(
                PiErrorKind::InvalidInput,
                "edit 工具输入必须包含 `\\n---\\n` 分隔符",
            )
        })?;
        permissions.require(request(Capability::WriteFile, path, "编辑文件"))?;
        let current = fs::read_to_string(path)?;
        let edited = current.replacen(old, new, 1);
        if edited == current {
            return Err(PiError::new(
                PiErrorKind::Tool,
                "edit 未找到要替换的文本",
            ));
        }
        fs::write(path, edited)?;
        Ok(output_for("edit", format!("已编辑 {path}")))
    }
}

impl Tool for BashTool {
    fn schema(&self) -> ToolSchema {
        schema("bash", "执行 shell 命令", "command", true)
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        permissions.require(request(Capability::ExecuteCommand, input, "执行命令"))?;
        let output = Command::new("sh").arg("-c").arg(input).output()?;
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(output_for("bash", text))
    }
}

impl Tool for EpkgTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "epkg",
            "规划 epkg/openEuler 包管理操作，输入为 epkg 子命令",
            "subcommand",
            true,
        )
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        permissions.require(request(
            Capability::ExecuteCommand,
            input,
            "规划 epkg/openEuler 包管理操作",
        ))?;
        Ok(output_for(
            "epkg",
            format!(
                "epkg/openEuler 操作已进入计划阶段，MVP 不直接修改系统。建议命令：epkg {input}"
            ),
        ))
    }
}

impl Tool for SearchTool {
    fn schema(&self) -> ToolSchema {
        schema("search", "递归搜索文本，输入格式为 path\\npattern", "path\\npattern", false)
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let (path, pattern) = split_once_line(input)?;
        permissions.require(request(Capability::ReadFile, path, "搜索文件"))?;
        let mut matches = Vec::new();
        search_path(Path::new(path), pattern, &mut matches)?;
        Ok(output_for("search", matches.join("\n")))
    }
}

impl Tool for LsTool {
    fn schema(&self) -> ToolSchema {
        schema("ls", "列出目录内容", "path", false)
    }

    fn run(&self, input: &str, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        permissions.require(request(Capability::ReadFile, input, "列出目录"))?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(input)? {
            let entry = entry?;
            let path = entry.path();
            let suffix = if path.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
        }
        entries.sort();
        Ok(output_for("ls", entries.join("\n")))
    }
}

fn schema(name: &str, description: &str, input_shape: &str, mutates: bool) -> ToolSchema {
    ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        input_shape: input_shape.to_string(),
        mutates,
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

fn search_path(path: &Path, pattern: &str, matches: &mut Vec<String>) -> PiResult<()> {
    if path.is_file() {
        let content = fs::read_to_string(path).unwrap_or_default();
        for (idx, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                matches.push(format!("{}:{}:{}", path.display(), idx + 1, line));
            }
        }
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child: PathBuf = entry.path();
        if child.is_dir() {
            search_path(&child, pattern, matches)?;
        } else if child.is_file() {
            search_path(&child, pattern, matches)?;
        }
    }
    Ok(())
}
