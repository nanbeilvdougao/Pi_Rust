//! Workspace hooks.
//!
//! Lifecycle: the agent runs project-specific shell hooks at four points:
//!
//! - `pre-turn`  — before the user prompt is dispatched to the provider.
//! - `pre-tool`  — before each tool call. Non-zero exit aborts the call.
//! - `post-tool` — after each tool call, success or failure. Output is
//!   logged; the exit code is advisory only.
//! - `post-turn` — after the assistant message lands. Same advisory rule.
//!
//! Hooks live at `<workspace>/.pi/hooks/{pre-turn,pre-tool,post-tool,post-turn}.sh`.
//! On Windows we also look for `.cmd` / `.bat` variants. Missing files are
//! treated as a no-op.
//!
//! Environment variables passed to every hook:
//!
//! - `PI_PHASE` — one of `pre-turn` / `pre-tool` / `post-tool` / `post-turn`.
//! - `PI_PROMPT` — the user prompt for turn-level hooks (the FIRST 4 KiB
//!   so the env stays within OS limits).
//! - `PI_TOOL_NAME` — the tool being called (tool-level hooks only).
//! - `PI_TOOL_INPUT` — the tool's raw JSON input (first 4 KiB).
//! - `PI_TOOL_OUTPUT` — the tool's output (post-tool only, first 4 KiB).
//! - `PI_SESSION_ID` — the current session id.
//!
//! Stdout from a hook becomes a `Event::ToolStarted{name:"hook:<phase>"}`
//! + `Event::ToolFinished` pair so the TUI shows it inline; stderr is
//! treated the same way but flagged as a warning.
//!
//! Parity target: `packages/agent/src/hooks.ts`.

use std::path::{Path, PathBuf};
use std::process::Command;

use pi_core::{PiError, PiErrorKind, PiResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    PreTurn,
    PreTool,
    PostTool,
    PostTurn,
}

impl HookPhase {
    pub fn slug(self) -> &'static str {
        match self {
            HookPhase::PreTurn => "pre-turn",
            HookPhase::PreTool => "pre-tool",
            HookPhase::PostTool => "post-tool",
            HookPhase::PostTurn => "post-turn",
        }
    }
}

/// Inputs the agent passes when running a hook.
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: String,
    pub prompt: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<String>,
    pub tool_output: Option<String>,
}

/// Result of running a single hook.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub ran: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl HookOutcome {
    /// Whether the hook signaled an abort. `pre-*` hooks abort on any
    /// non-zero exit; `post-*` hooks are advisory.
    pub fn aborts(&self, phase: HookPhase) -> bool {
        matches!(phase, HookPhase::PreTurn | HookPhase::PreTool) && self.exit_code != 0
    }
}

/// Resolve the on-disk script for `phase` if any exists.
pub fn resolve(workspace: &Path, phase: HookPhase) -> Option<PathBuf> {
    let dir = workspace.join(".pi").join("hooks");
    let base = phase.slug();
    let candidates = if cfg!(target_os = "windows") {
        vec![format!("{base}.cmd"), format!("{base}.bat"), format!("{base}.sh")]
    } else {
        vec![base.to_string(), format!("{base}.sh")]
    };
    for name in candidates {
        let path = dir.join(&name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

const MAX_ENV_BYTES: usize = 4096;

fn truncate_env(value: &str) -> String {
    if value.len() <= MAX_ENV_BYTES {
        return value.to_string();
    }
    let mut idx = MAX_ENV_BYTES;
    while idx > 0 && !value.is_char_boundary(idx) {
        idx -= 1;
    }
    value[..idx].to_string()
}

/// Run the hook for `phase` if one is on disk. Returns `HookOutcome::ran=false`
/// when no script exists, so callers can branch without parsing exit codes.
pub fn run(workspace: &Path, phase: HookPhase, ctx: &HookContext) -> PiResult<HookOutcome> {
    let Some(path) = resolve(workspace, phase) else {
        return Ok(HookOutcome {
            ran: false,
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
    };
    let mut command = if cfg!(target_os = "windows")
        && (path.extension().and_then(|e| e.to_str()) == Some("cmd")
            || path.extension().and_then(|e| e.to_str()) == Some("bat"))
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&path);
        c
    } else {
        Command::new(&path)
    };
    command.current_dir(workspace);
    command.env("PI_PHASE", phase.slug());
    if !ctx.session_id.is_empty() {
        command.env("PI_SESSION_ID", &ctx.session_id);
    }
    if let Some(prompt) = &ctx.prompt {
        command.env("PI_PROMPT", truncate_env(prompt));
    }
    if let Some(name) = &ctx.tool_name {
        command.env("PI_TOOL_NAME", name);
    }
    if let Some(input) = &ctx.tool_input {
        command.env("PI_TOOL_INPUT", truncate_env(input));
    }
    if let Some(output) = &ctx.tool_output {
        command.env("PI_TOOL_OUTPUT", truncate_env(output));
    }
    let output = command.output().map_err(|err| {
        PiError::new(
            PiErrorKind::Io,
            format!("hook {} 启动失败：{err}", phase.slug()),
        )
    })?;
    Ok(HookOutcome {
        ran: true,
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_hook(dir: &TempDir, phase: HookPhase, script: &str) {
        let hook_dir = dir.path().join(".pi").join("hooks");
        std::fs::create_dir_all(&hook_dir).unwrap();
        let path = hook_dir.join(phase.slug());
        std::fs::write(&path, script).unwrap();
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
    }

    #[test]
    fn no_op_when_hook_missing() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(dir.path(), HookPhase::PreTool, &HookContext::default())
            .expect("run");
        assert!(!outcome.ran);
        assert_eq!(outcome.exit_code, 0);
    }

    #[test]
    fn pre_tool_aborts_when_exit_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        write_hook(&dir, HookPhase::PreTool, "#!/bin/sh\nexit 7\n");
        let outcome = run(dir.path(), HookPhase::PreTool, &HookContext::default())
            .expect("run");
        assert!(outcome.ran);
        assert_eq!(outcome.exit_code, 7);
        assert!(outcome.aborts(HookPhase::PreTool));
    }

    #[test]
    fn post_tool_exit_nonzero_does_not_abort() {
        let dir = tempfile::tempdir().unwrap();
        write_hook(&dir, HookPhase::PostTool, "#!/bin/sh\nexit 9\n");
        let outcome = run(dir.path(), HookPhase::PostTool, &HookContext::default())
            .expect("run");
        assert!(outcome.ran);
        assert_eq!(outcome.exit_code, 9);
        assert!(!outcome.aborts(HookPhase::PostTool));
    }

    #[test]
    fn env_vars_reach_hook_script() {
        let dir = tempfile::tempdir().unwrap();
        write_hook(
            &dir,
            HookPhase::PreTool,
            "#!/bin/sh\necho phase=$PI_PHASE tool=$PI_TOOL_NAME\n",
        );
        let outcome = run(
            dir.path(),
            HookPhase::PreTool,
            &HookContext {
                tool_name: Some("read".into()),
                ..HookContext::default()
            },
        )
        .expect("run");
        assert!(outcome.stdout.contains("phase=pre-tool"), "got: {}", outcome.stdout);
        assert!(outcome.stdout.contains("tool=read"));
    }

    #[test]
    fn long_inputs_get_truncated_before_env() {
        let huge = "x".repeat(MAX_ENV_BYTES * 4);
        let truncated = truncate_env(&huge);
        assert_eq!(truncated.len(), MAX_ENV_BYTES);
    }
}
