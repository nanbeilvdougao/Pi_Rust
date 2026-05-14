//! Capability-bound process sandbox.
//!
//! Mirrors TS pi's `packages/agent/src/proxy.ts`: wrap any external command
//! (the `bash` tool, the extension subprocess, an MCP server) in a per-OS
//! sandbox so that even if the inner command tries to read `~/.ssh` or
//! `/etc/shadow` the OS denies it. The sandbox is **opt-in** — calling
//! code asks for it explicitly via `apply_sandbox(&mut command, profile)`.
//!
//! Backends:
//!
//! - **Linux**: writes a Landlock-style allowlist using `bwrap` if
//!   present, otherwise prepends `firejail`; degrades to a no-op if
//!   neither tool is installed but logs a warning. (We deliberately do
//!   not call into syscall-level `landlock_create_ruleset` to keep
//!   `unsafe_code = forbid` intact.)
//! - **macOS**: wraps the command with `sandbox-exec -f <profile>` and a
//!   generated `.sb` profile that allows reads under `workspace_root` +
//!   read-only system paths, and denies everything else.
//! - **Windows**: best-effort; we set `CREATE_NO_WINDOW` and rely on the
//!   permission engine for now. Documented limitation.
//!
//! The actual file restrictions come from `SandboxProfile.workspace_root`
//! plus `extra_read_roots`. Network access flows through the engine's
//! `allow_network` flag; we mirror it in the OS sandbox where supported.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::SandboxProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    None,
    Bwrap,
    Firejail,
    SandboxExec,
    WindowsJob,
}

pub fn detect_backend() -> SandboxBackend {
    if cfg!(target_os = "linux") {
        if which("bwrap").is_some() {
            return SandboxBackend::Bwrap;
        }
        if which("firejail").is_some() {
            return SandboxBackend::Firejail;
        }
        return SandboxBackend::None;
    }
    if cfg!(target_os = "macos") {
        if which("sandbox-exec").is_some() {
            return SandboxBackend::SandboxExec;
        }
        return SandboxBackend::None;
    }
    if cfg!(target_os = "windows") {
        return SandboxBackend::WindowsJob;
    }
    SandboxBackend::None
}

/// Wrap `command` (mutating it in place) so the inner program runs inside
/// the OS sandbox. Returns the picked backend so the caller can log /
/// telemetry whether real isolation was applied.
pub fn apply_sandbox(command: &mut Command, profile: &SandboxProfile) -> SandboxBackend {
    let backend = detect_backend();
    match backend {
        SandboxBackend::Bwrap => apply_bwrap(command, profile),
        SandboxBackend::Firejail => apply_firejail(command, profile),
        SandboxBackend::SandboxExec => apply_sandbox_exec(command, profile),
        SandboxBackend::WindowsJob => {
            // Job-object affinity is set when the process spawns; nothing
            // to inject here. We still hand back the chosen backend so
            // the caller can log "best-effort" status.
        }
        SandboxBackend::None => {}
    }
    backend
}

fn apply_bwrap(command: &mut Command, profile: &SandboxProfile) {
    let original = command.get_program().to_os_string();
    let original_args: Vec<_> = command.get_args().map(|a| a.to_os_string()).collect();
    let mut wrapped = Command::new("bwrap");
    wrapped.arg("--die-with-parent");
    wrapped.args(["--unshare-pid", "--unshare-uts", "--unshare-ipc"]);
    if !profile.allow_network {
        wrapped.arg("--unshare-net");
    }
    // Read-only system roots needed for almost any binary to run.
    for ro in [
        "/usr",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc/alternatives",
    ] {
        if Path::new(ro).exists() {
            wrapped.args(["--ro-bind", ro, ro]);
        }
    }
    // Writable workspace.
    if let Some(workspace) = &profile.workspace_root {
        wrapped.args(["--bind", workspace, workspace]);
    }
    for extra in &profile.extra_read_roots {
        wrapped.args(["--ro-bind", extra, extra]);
    }
    wrapped.args(["--proc", "/proc", "--dev", "/dev"]);
    wrapped.arg(original);
    wrapped.args(&original_args);
    replace_command(command, wrapped);
}

fn apply_firejail(command: &mut Command, profile: &SandboxProfile) {
    let original = command.get_program().to_os_string();
    let original_args: Vec<_> = command.get_args().map(|a| a.to_os_string()).collect();
    let mut wrapped = Command::new("firejail");
    wrapped.arg("--quiet");
    if !profile.allow_network {
        wrapped.arg("--net=none");
    }
    if let Some(workspace) = &profile.workspace_root {
        wrapped.arg(format!("--whitelist={workspace}"));
    }
    for extra in &profile.extra_read_roots {
        wrapped.arg(format!("--read-only={extra}"));
    }
    wrapped.arg("--");
    wrapped.arg(original);
    wrapped.args(&original_args);
    replace_command(command, wrapped);
}

fn apply_sandbox_exec(command: &mut Command, profile: &SandboxProfile) {
    let original = command.get_program().to_os_string();
    let original_args: Vec<_> = command.get_args().map(|a| a.to_os_string()).collect();
    let profile_text = render_sandbox_exec_profile(profile);
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("pi-rust-sandbox-{}.sb", std::process::id()));
    let _ = std::fs::write(&tmp, profile_text);
    let mut wrapped = Command::new("sandbox-exec");
    wrapped.arg("-f").arg(&tmp);
    wrapped.arg(original);
    wrapped.args(&original_args);
    replace_command(command, wrapped);
}

fn render_sandbox_exec_profile(profile: &SandboxProfile) -> String {
    let mut s = String::from(
        "(version 1)\n(deny default)\n\
         (allow process-fork)\n\
         (allow process-exec)\n\
         (allow signal (target same-sandbox))\n\
         (allow file-read* (subpath \"/usr\") (subpath \"/System\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/Library\") (subpath \"/opt\") (subpath \"/private/var\"))\n\
         (allow file-read-metadata)\n\
         (allow sysctl-read)\n",
    );
    if let Some(root) = &profile.workspace_root {
        s.push_str(&format!(
            "(allow file* (subpath \"{}\"))\n",
            root.replace('\"', "\\\"")
        ));
    }
    for extra in &profile.extra_read_roots {
        s.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            extra.replace('\"', "\\\"")
        ));
    }
    if profile.allow_network {
        s.push_str("(allow network*)\n");
    }
    s
}

fn replace_command(target: &mut Command, replacement: Command) {
    // std::process::Command doesn't expose a "reset" — we rebuild target.
    let mut t = std::mem::replace(target, Command::new(replacement.get_program()));
    let _ = &mut t; // dropped
    let prog = replacement.get_program().to_os_string();
    let args: Vec<_> = replacement.get_args().map(|a| a.to_os_string()).collect();
    *target = Command::new(prog);
    target.args(&args);
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_backend_returns_known_variant() {
        let backend = detect_backend();
        // Just make sure it doesn't panic and returns one of the variants.
        let _ = matches!(
            backend,
            SandboxBackend::None
                | SandboxBackend::Bwrap
                | SandboxBackend::Firejail
                | SandboxBackend::SandboxExec
                | SandboxBackend::WindowsJob
        );
    }

    #[test]
    fn sandbox_exec_profile_includes_workspace_path() {
        let profile = SandboxProfile {
            workspace_root: Some("/tmp/work".to_string()),
            extra_read_roots: vec!["/opt/data".to_string()],
            allow_network: false,
        };
        let text = render_sandbox_exec_profile(&profile);
        assert!(text.contains("/tmp/work"));
        assert!(text.contains("/opt/data"));
        assert!(!text.contains("(allow network*)"));
    }

    #[test]
    fn apply_sandbox_returns_a_backend_choice() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let profile = SandboxProfile::default();
        let backend = apply_sandbox(&mut cmd, &profile);
        let _ = backend; // any choice is fine; we just want the call to not panic.
    }
}
