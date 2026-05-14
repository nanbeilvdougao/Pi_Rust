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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pi_core::{PiError, PiErrorKind, PiResult};

/// Per-phase hook execution budget. A hung script counts as a non-zero exit
/// so the existing `aborts()` semantics keep pre-* hooks fail-closed.
const PRE_HOOK_TIMEOUT: Duration = Duration::from_secs(30);
const POST_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

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
    let timeout = match phase {
        HookPhase::PreTurn | HookPhase::PreTool => PRE_HOOK_TIMEOUT,
        HookPhase::PostTool | HookPhase::PostTurn => POST_HOOK_TIMEOUT,
    };
    run_with_timeout(workspace, phase, ctx, timeout)
}

/// Same as `run` but the caller supplies the timeout budget. Exposed crate-
/// internally so unit tests can exercise the timeout branch without waiting
/// the full 30/60 second default.
pub(crate) fn run_with_timeout(
    workspace: &Path,
    phase: HookPhase,
    ctx: &HookContext,
    timeout: Duration,
) -> PiResult<HookOutcome> {
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
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| {
        PiError::new(
            PiErrorKind::Io,
            format!("hook {} 启动失败：{err}", phase.slug()),
        )
    })?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|err| {
            PiError::new(PiErrorKind::Io, format!("hook 等待失败：{err}"))
        })? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let exit_code = match status {
        Some(s) => s.code().unwrap_or(-1),
        None => {
            // Kill the runaway hook so it cannot continue holding resources
            // after we move on. Treat the timeout as a non-zero exit so
            // pre-* hooks fail-closed.
            let _ = child.kill();
            let _ = child.wait();
            -1
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    if status.is_none() {
        stderr.push_str(&format!(
            "\n[hook] 超时 {} 秒，已强制终止",
            timeout.as_secs()
        ));
    }
    Ok(HookOutcome {
        ran: true,
        exit_code,
        stdout,
        stderr,
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
    fn hung_pre_tool_hook_times_out_and_aborts() {
        // Drop the timeout to keep the test fast. We bypass the public
        // constant by writing a sleep that vastly exceeds it; the timeout
        // path triggers, the hook is treated as aborting.
        let dir = tempfile::tempdir().unwrap();
        // A pre-tool script that sleeps 120s. Our timeout is 30s, but for
        // the test we exercise the cancel branch by directly invoking with
        // a hook that exits 1 immediately when SIGTERM is received.
        write_hook(
            &dir,
            HookPhase::PreTool,
            "#!/bin/sh\nsleep 120\nexit 0\n",
        );
        // Use a custom internal helper to avoid waiting 30s in the test.
        let start = std::time::Instant::now();
        let outcome = run_with_timeout(
            dir.path(),
            HookPhase::PreTool,
            &HookContext::default(),
            std::time::Duration::from_millis(200),
        )
        .expect("run");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout path took too long: {:?}",
            elapsed
        );
        assert_eq!(outcome.exit_code, -1);
        assert!(outcome.aborts(HookPhase::PreTool));
        assert!(outcome.stderr.contains("超时"));
    }

    #[test]
    fn long_inputs_get_truncated_before_env() {
        let huge = "x".repeat(MAX_ENV_BYTES * 4);
        let truncated = truncate_env(&huge);
        assert_eq!(truncated.len(), MAX_ENV_BYTES);
    }
}
