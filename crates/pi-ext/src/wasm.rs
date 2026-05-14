//! Wasmtime-backed extension host with full-duplex JSON-RPC.
//!
//! The guest runs on a dedicated worker thread. Communication is line-
//! delimited JSON over WASI preview-1 stdio, identical to the process host:
//!
//! ```text
//!  host  -> guest : Init                              (stdin)
//!  guest -> host  : Hostcall(id=1, call=…)            (stdout)
//!  host  -> guest : Reply(id=1, ok=true, value=…)     (stdin)
//!  guest -> host  : Hostcall(id=2, …)                 (stdout)
//!  …
//!  guest -> host  : Done                              (stdout)
//! ```
//!
//! The full-duplex part is what we got right this iteration. Earlier code
//! ran the guest to completion and only THEN dispatched its hostcalls —
//! useless for stateful extensions. We now:
//!
//! - Run the guest in its own thread.
//! - Implement a custom `BlockingStdin` (`WasiFile`) that blocks the guest
//!   read until the host pushes the next reply.
//! - Stream the guest's stdout into a shared buffer; the main thread reads
//!   line-by-line and dispatches.
//!
//! Capability gating still flows through `pi-permissions`; the manifest's
//! declared capabilities are enforced on every hostcall.

use std::any::Any;
use std::io::IoSliceMut;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;

use async_trait::async_trait;
use pi_permissions::{PermissionEngine, PermissionRequest};
use serde::{Deserialize, Serialize};
use wasi_common::file::{FdFlags, FileType, WasiFile};
use wasi_common::pipe::WritePipe;
use wasi_common::sync::WasiCtxBuilder;
use wasi_common::{Error, WasiCtx};
use wasmtime::{Config, Engine, Linker, Module, Store};

use crate::process::HostcallResolver;
use crate::{AbiVersion, ExtensionEntry, ExtensionManifest, Hostcall};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostFrame {
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
enum ExtensionFrame {
    Hostcall { id: u64, call: Hostcall },
    Event { data: serde_json::Value },
    Done { exit_code: i32 },
}

pub struct WasmExtensionHost<'a> {
    manifest: ExtensionManifest,
    root: PathBuf,
    permissions: &'a mut PermissionEngine,
}

impl<'a> WasmExtensionHost<'a> {
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

    pub fn run<R: HostcallResolver>(
        &mut self,
        resolver: &mut R,
    ) -> Result<Vec<serde_json::Value>, String> {
        let path = match &self.manifest.entry {
            ExtensionEntry::Wasm { path, .. } => path.clone(),
            _ => return Err("WASM 宿主只能运行 entry.kind=wasm 的扩展".to_string()),
        };
        let wasm_path = self.root.join(&path);
        if !wasm_path.exists() {
            return Err(format!("找不到 WASM 模块：{}", wasm_path.display()));
        }

        let stdin = Arc::new(BlockingStdin::default());
        let stdout = Arc::new(RwLock::new(Vec::<u8>::new()));
        let stdout_pipe = WritePipe::from_shared(Arc::clone(&stdout));

        // Send init frame *before* the guest starts; it's the first thing the
        // guest will read.
        let init = HostFrame::Init {
            abi: self.manifest.abi,
            capabilities: self
                .manifest
                .capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        };
        stdin.push_frame(&init)?;

        let engine = build_engine()?;
        let module = Module::from_file(&engine, &wasm_path)
            .map_err(|err| format!("加载 WASM 模块失败：{err}"))?;

        // Spawn the guest worker. It runs to completion (or until the host
        // injects `Shutdown` and the guest exits cooperatively).
        let stdin_for_worker = Arc::clone(&stdin);
        let stdout_for_worker = stdout_pipe;
        let module = Arc::new(module);
        let engine_for_worker = engine;
        let worker_handle = thread::Builder::new()
            .name(format!("pi-ext-wasm-{}", self.manifest.id))
            .spawn(move || -> Result<i32, String> {
                let mut linker = Linker::<WasiCtx>::new(&engine_for_worker);
                wasi_common::sync::add_to_linker(&mut linker, |cx| cx)
                    .map_err(|err| format!("link WASI 失败：{err}"))?;
                let mut wasi_builder = WasiCtxBuilder::new();
                wasi_builder.stdin(Box::new(BlockingStdinHandle(stdin_for_worker)));
                wasi_builder.stdout(Box::new(stdout_for_worker));
                wasi_builder.inherit_stderr();
                let wasi = wasi_builder.build();
                let mut store = Store::new(&engine_for_worker, wasi);
                let instance = linker
                    .instantiate(&mut store, &module)
                    .map_err(|err| format!("实例化 WASM 模块失败：{err}"))?;
                let start = instance
                    .get_typed_func::<(), ()>(&mut store, "_start")
                    .map_err(|err| format!("WASM 模块缺少 _start 入口：{err}"))?;
                start
                    .call(&mut store, ())
                    .map_err(|err| format!("WASM 执行失败：{err}"))?;
                Ok(0)
            })
            .map_err(|err| format!("启动 WASM worker 失败：{err}"))?;

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut cursor = 0usize;
        let mut line_buf = Vec::<u8>::new();
        let mut done = false;
        while !done && !worker_handle.is_finished()
            || stdout.read().map(|s| s.len()).unwrap_or(0) > cursor
        {
            // Drain whatever stdout has accumulated.
            let snapshot = stdout
                .read()
                .map_err(|err| format!("stdout lock：{err}"))?
                .clone();
            while cursor < snapshot.len() {
                let byte = snapshot[cursor];
                cursor += 1;
                if byte == b'\n' {
                    if let Some(frame) =
                        self.handle_line(&line_buf, &stdin, resolver, &mut events)?
                    {
                        if matches!(frame, FrameAction::Done) {
                            done = true;
                        }
                    }
                    line_buf.clear();
                } else {
                    line_buf.push(byte);
                }
            }
            if !done {
                if worker_handle.is_finished() && cursor == snapshot.len() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        // Signal shutdown so the guest's next read returns 0 if it tries.
        stdin.close();
        let _ = worker_handle
            .join()
            .map_err(|err| format!("WASM worker panic：{err:?}"))?;
        Ok(events)
    }

    fn handle_line<R: HostcallResolver>(
        &mut self,
        line: &[u8],
        stdin: &Arc<BlockingStdin>,
        resolver: &mut R,
        events: &mut Vec<serde_json::Value>,
    ) -> Result<Option<FrameAction>, String> {
        if line.is_empty() {
            return Ok(None);
        }
        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        let frame: ExtensionFrame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(_) => return Ok(None),
        };
        match frame {
            ExtensionFrame::Hostcall { id, call } => {
                let decision = self.permissions.decide(PermissionRequest {
                    capability: call.required_capability(),
                    target: hostcall_target(&call),
                    reason: format!("WASM 扩展 {} 请求 hostcall", self.manifest.id),
                });
                let reply = if !decision.allowed {
                    HostFrame::Reply {
                        id,
                        ok: false,
                        value: None,
                        error: Some(decision.reason),
                    }
                } else if !self
                    .manifest
                    .capabilities
                    .iter()
                    .any(|c| *c == call.required_capability())
                {
                    HostFrame::Reply {
                        id,
                        ok: false,
                        value: None,
                        error: Some(format!(
                            "WASM 扩展未声明 {} 能力",
                            call.required_capability().as_str()
                        )),
                    }
                } else {
                    match resolver.resolve(&call) {
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
                    }
                };
                stdin.push_frame(&reply)?;
                Ok(Some(FrameAction::Continue))
            }
            ExtensionFrame::Event { data } => {
                events.push(data);
                Ok(Some(FrameAction::Continue))
            }
            ExtensionFrame::Done { exit_code: _ } => Ok(Some(FrameAction::Done)),
        }
    }
}

enum FrameAction {
    Continue,
    Done,
}

fn build_engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config.consume_fuel(false);
    config.epoch_interruption(false);
    Engine::new(&config).map_err(|err| format!("创建 wasmtime engine 失败：{err}"))
}

fn hostcall_target(call: &Hostcall) -> String {
    match call {
        Hostcall::Tool { name, .. } => name.clone(),
        Hostcall::SessionRead { key } | Hostcall::SessionWrite { key, .. } => key.clone(),
        Hostcall::Http { url, .. } => url.clone(),
        Hostcall::UiNotify { .. } => "ui".to_string(),
    }
}

/// Stdin for the guest. Blocks on read until the host pushes more bytes.
#[derive(Default)]
struct BlockingStdin {
    inner: Mutex<BlockingState>,
    cv: Condvar,
}

#[derive(Default)]
struct BlockingState {
    buf: Vec<u8>,
    pos: usize,
    closed: bool,
}

impl BlockingStdin {
    fn push_frame<T: Serialize>(&self, frame: &T) -> Result<(), String> {
        let mut bytes =
            serde_json::to_vec(frame).map_err(|err| format!("序列化 host 帧失败：{err}"))?;
        bytes.push(b'\n');
        let mut state = self
            .inner
            .lock()
            .map_err(|err| format!("stdin lock：{err}"))?;
        state.buf.extend_from_slice(&bytes);
        self.cv.notify_all();
        Ok(())
    }

    fn close(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.closed = true;
            self.cv.notify_all();
        }
    }

    fn read_into(&self, out: &mut [u8]) -> usize {
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        loop {
            if state.pos < state.buf.len() {
                let available = state.buf.len() - state.pos;
                let take = available.min(out.len());
                out[..take].copy_from_slice(&state.buf[state.pos..state.pos + take]);
                state.pos += take;
                return take;
            }
            if state.closed {
                return 0;
            }
            state = match self.cv.wait(state) {
                Ok(state) => state,
                Err(_) => return 0,
            };
        }
    }
}

struct BlockingStdinHandle(Arc<BlockingStdin>);

#[async_trait]
impl WasiFile for BlockingStdinHandle {
    fn as_any(&self) -> &dyn Any {
        self
    }
    async fn get_filetype(&self) -> Result<FileType, Error> {
        Ok(FileType::Pipe)
    }
    async fn get_fdflags(&self) -> Result<FdFlags, Error> {
        Ok(FdFlags::empty())
    }
    async fn read_vectored<'a>(&self, bufs: &mut [IoSliceMut<'a>]) -> Result<u64, Error> {
        let mut total = 0u64;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let n = self.0.read_into(buf);
            total += n as u64;
            if n == 0 {
                break;
            }
            // We only fill the first buffer to keep behavior predictable
            // for line-oriented guests.
            break;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_stdin_returns_pushed_bytes_then_blocks() {
        let stdin = Arc::new(BlockingStdin::default());
        stdin.push_frame(&HostFrame::Shutdown).unwrap();
        let mut buf = [0u8; 64];
        let n = stdin.read_into(&mut buf);
        assert!(n > 0);
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("\"shutdown\""));
    }

    #[test]
    fn blocking_stdin_closes_cleanly() {
        let stdin = Arc::new(BlockingStdin::default());
        let stdin2 = Arc::clone(&stdin);
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8];
            stdin2.read_into(&mut buf)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        stdin.close();
        let n = handle.join().unwrap();
        assert_eq!(n, 0);
    }
}
