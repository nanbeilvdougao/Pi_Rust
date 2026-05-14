//! Process-backed extension host.
//!
//! Protocol: line-delimited JSON over stdin/stdout. Each line is one message.
//! The host writes a single "init" envelope first containing the ABI version
//! and granted capabilities; the extension then alternates sending hostcall
//! requests and receiving replies.
//!
//! Frame shape (host -> extension):
//! ```json
//! {"kind":"init","abi":{"major":1,"minor":0},"capabilities":["execute_command"]}
//! {"kind":"reply","id":42,"ok":true,"value":...}
//! {"kind":"shutdown"}
//! ```
//!
//! Frame shape (extension -> host):
//! ```json
//! {"kind":"hostcall","id":42,"call":{"type":"http","method":"GET","url":"..."}}
//! {"kind":"event","data":{"text":"..."}}
//! {"kind":"done","exit_code":0}
//! ```
//!
//! The host enforces capabilities on every `hostcall` envelope by routing
//! through `PermissionEngine::require`. If the permission is denied, the host
//! sends `{kind:"reply", ok:false, error:"..."}` and the extension is expected
//! to surface the failure to the user (or retry under a different mode).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use pi_permissions::{PermissionEngine, PermissionRequest};
use serde::{Deserialize, Serialize};

use crate::{AbiVersion, ExtensionEntry, ExtensionManifest, Hostcall};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostFrame {
    Init {
        abi: AbiVersion,
        capabilities: Vec<String>,
    },
    Reply {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionFrame {
    Hostcall { id: u64, call: Hostcall },
    Event { data: serde_json::Value },
    Done { exit_code: i32 },
}

#[derive(Debug, Default, Clone)]
pub struct HostcallReply {
    pub ok: bool,
    pub value: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ExtensionInstance {
    child: Child,
    stdin: ChildStdin,
}

/// Trait the agent implements so the extension host can resolve hostcalls
/// against real services (tools, session, network). Implementations should
/// *not* perform permission checks themselves — the host already gates them.
pub trait HostcallResolver {
    fn resolve(&mut self, call: &Hostcall) -> Result<serde_json::Value, String>;
}

pub struct ExtensionHost<'a> {
    manifest: ExtensionManifest,
    root: PathBuf,
    permissions: &'a mut PermissionEngine,
}

impl<'a> ExtensionHost<'a> {
    pub fn new(
        manifest: ExtensionManifest,
        root: PathBuf,
        permissions: &'a mut PermissionEngine,
    ) -> Self {
        Self {
            manifest,
            root,
            permissions,
        }
    }

    pub fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    /// Launch the extension subprocess and run the protocol to completion,
    /// dispatching hostcalls through `resolver`.
    pub fn run<R: HostcallResolver>(
        &mut self,
        resolver: &mut R,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut instance = self.spawn()?;
        let stdout = instance.child.stdout.take().ok_or("缺少子进程 stdout")?;
        let mut reader = BufReader::new(stdout);

        // Send init envelope.
        let init = HostFrame::Init {
            abi: self.manifest.abi,
            capabilities: self
                .manifest
                .capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        };
        write_frame(&mut instance.stdin, &init)?;

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader
                .read_line(&mut line)
                .map_err(|err| format!("读取扩展输出失败：{err}"))?;
            if read == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }
            let frame: ExtensionFrame = match serde_json::from_str(trimmed) {
                Ok(frame) => frame,
                Err(err) => {
                    let reply = HostFrame::Reply {
                        id: 0,
                        ok: false,
                        value: None,
                        error: Some(format!("无效帧：{err}; line={trimmed}")),
                    };
                    let _ = write_frame(&mut instance.stdin, &reply);
                    continue;
                }
            };

            match frame {
                ExtensionFrame::Hostcall { id, call } => {
                    let request = PermissionRequest {
                        capability: call.required_capability(),
                        target: hostcall_target(&call),
                        reason: format!("扩展 {} 请求 hostcall", self.manifest.id),
                    };
                    let decision = self.permissions.decide(request);
                    if !decision.allowed {
                        write_frame(
                            &mut instance.stdin,
                            &HostFrame::Reply {
                                id,
                                ok: false,
                                value: None,
                                error: Some(decision.reason),
                            },
                        )?;
                        continue;
                    }
                    if !self
                        .manifest
                        .capabilities
                        .iter()
                        .any(|c| *c == call.required_capability())
                    {
                        write_frame(
                            &mut instance.stdin,
                            &HostFrame::Reply {
                                id,
                                ok: false,
                                value: None,
                                error: Some(format!(
                                    "扩展 manifest 未声明 {} 能力",
                                    call.required_capability().as_str()
                                )),
                            },
                        )?;
                        continue;
                    }
                    let result = resolver.resolve(&call);
                    let reply = match result {
                        Ok(value) => HostFrame::Reply {
                            id,
                            ok: true,
                            value: Some(value),
                            error: None,
                        },
                        Err(err) => HostFrame::Reply {
                            id,
                            ok: false,
                            value: None,
                            error: Some(err),
                        },
                    };
                    write_frame(&mut instance.stdin, &reply)?;
                }
                ExtensionFrame::Event { data } => events.push(data),
                ExtensionFrame::Done { exit_code: _ } => {
                    let _ = write_frame(&mut instance.stdin, &HostFrame::Shutdown);
                    break;
                }
            }
        }

        let _ = instance.child.wait();
        Ok(events)
    }

    fn spawn(&self) -> Result<ExtensionInstance, String> {
        let mut command = build_command(&self.manifest.entry, &self.root);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|err| format!("启动扩展进程失败：{err}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "缺少子进程 stdin".to_string())?;
        Ok(ExtensionInstance { child, stdin })
    }
}

fn build_command(entry: &ExtensionEntry, root: &Path) -> Command {
    match entry {
        ExtensionEntry::Process { command, args, env } => {
            let mut cmd = Command::new(command);
            cmd.args(args).current_dir(root);
            for (key, value) in env {
                cmd.env(key, value);
            }
            cmd
        }
        ExtensionEntry::Shell { command } => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command).current_dir(root);
            cmd
        }
        ExtensionEntry::Wasm { .. } => {
            // WASM entries are handled by `crate::wasm::WasmExtensionHost`
            // when the `wasm` feature is enabled. The process host returns
            // a no-op command that will fail to spawn so callers must route
            // through the right host explicitly.
            Command::new("false")
        }
    }
}

fn write_frame<W: Write, T: Serialize>(writer: &mut W, frame: &T) -> Result<(), String> {
    let mut line =
        serde_json::to_string(frame).map_err(|err| format!("序列化 host 帧失败：{err}"))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|err| format!("写入扩展 stdin 失败：{err}"))?;
    writer.flush().map_err(|err| format!("flush 失败：{err}"))?;
    Ok(())
}

fn hostcall_target(call: &Hostcall) -> String {
    match call {
        Hostcall::Tool { name, .. } => name.clone(),
        Hostcall::SessionRead { key } | Hostcall::SessionWrite { key, .. } => key.clone(),
        Hostcall::Http { url, .. } => url.clone(),
        Hostcall::UiNotify { .. } => "ui".to_string(),
        Hostcall::ResourceList => "resources/list".to_string(),
        Hostcall::ResourceRead { uri } => uri.clone(),
        Hostcall::PromptList => "prompts/list".to_string(),
        Hostcall::PromptGet { name, .. } => name.clone(),
        Hostcall::NotifyEvent { method, .. } => method.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_permissions::{Capability, PermissionEngine, PermissionMode, SandboxProfile};
    use tempfile::tempdir;

    struct EchoResolver;
    impl HostcallResolver for EchoResolver {
        fn resolve(&mut self, call: &Hostcall) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"echoed": format!("{call:?}")}))
        }
    }

    #[test]
    fn host_runs_shell_extension_round_trip() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("ext.sh");
        // Extension reads init line, sends one hostcall, reads reply, sends done.
        std::fs::write(
            &script,
            r#"#!/usr/bin/env bash
set -e
read INIT
echo '{"kind":"hostcall","id":1,"call":{"type":"ui_notify","message":"hi"}}'
read REPLY
echo '{"kind":"event","data":{"ok":true}}'
echo '{"kind":"done","exit_code":0}'
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }

        let manifest = ExtensionManifest {
            id: "echo".to_string(),
            name: "Echo".to_string(),
            version: "0.0.1".to_string(),
            abi: AbiVersion::V1,
            entry: ExtensionEntry::Shell {
                command: format!("'{}'", script.display()),
            },
            capabilities: vec![Capability::ExtensionHostcall],
            description: None,
        };

        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace)
            .with_sandbox(SandboxProfile::default());
        let mut host = ExtensionHost::new(manifest, dir.path().to_path_buf(), &mut permissions);
        let mut resolver = EchoResolver;
        let events = host.run(&mut resolver).expect("run");
        assert_eq!(events.len(), 1);
    }
}
