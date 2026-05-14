# Pi Rust

Pi Rust 是一个从零设计的 Rust 版 Pi 终端 AI 编程助手。

目标：与 TypeScript 版 Pi 功能对齐，并在性能、可扩展性、设计哲学上超越已有的 Rust 移植（`pi_agent_rust`、`pi-rs`）。

## 已实现亮点

- **十个内置 provider**：echo、ollama、openai、moonshot、deepseek、qwen、zhipu、minimax、anthropic、gemini，全部支持流式与工具调用。
- **真实 SSE 流**：OpenAI 兼容协议、Anthropic Messages、Gemini SSE、Ollama NDJSON 全部走真实流式管道，统一为 `StreamEvent`。
- **能力权限引擎**：四种权限模式（`read-only` / `confirm` / `trusted` / `plan`）+ 审计日志 + 沙箱根目录 + 危险目标黑名单。
- **上下文压缩**：估算 token 超过窗口阈值时自动摘要中段消息，保留首尾对话与系统提示。
- **可扩展运行时**：基于子进程 + JSON-RPC 的 `pi-ext` 扩展宿主，能力声明 + 引擎双重把关。
- **真实 TUI**：ratatui + crossterm，流式渲染、`Ctrl+C` 协同取消、命令历史、token 用量条。
- **工具集对齐 TS 版**：read（行号/偏移/限制）、write（自动建目录）、edit（唯一匹配 + diff 预览）、bash（cwd / 超时）、grep（正则）、find（glob）、ls、epkg。

## 工程目录

- `crates/pi-core`：核心类型、消息、流式事件、Usage、权限枚举。
- `crates/pi-agent`：Agent 循环、工具调度、流式 sink、上下文压缩、slash 命令、系统提示构造。
- `crates/pi-providers`：Provider trait + 内置 provider 实现，按 provider 拆分到子模块。
- `crates/pi-tools`：内置工具、JSON schema、diff 预览、glob/regex 搜索。
- `crates/pi-permissions`：能力 + 审计 + 沙箱 + 危险命令黑名单。
- `crates/pi-session`：JSONL 会话存储，serde-derived，向后兼容旧手写格式。
- `crates/pi-tui`：ratatui+crossterm 的交互式终端 UI。
- `crates/pi-ext`：扩展 ABI v1 + 子进程 JSON-RPC 宿主。
- `crates/pi-cli`：`pi` 命令行入口，clap 解析。
- `crates/pi-bench`：criterion 基准。

## 使用

```bash
# 一次性输出
cargo run -p pi-cli -- -p "你好，介绍下这个项目"

# 进入交互式 TUI（默认 echo provider）
cargo run -p pi-cli --

# 切换到 Moonshot
export MOONSHOT_API_KEY=sk-...
cargo run -p pi-cli -- --provider moonshot --model moonshot-v1-8k

# 列出 provider / 模型 / 工具 / 会话
cargo run -p pi-cli -- --list-providers
cargo run -p pi-cli -- --list-models
cargo run -p pi-cli -- --list-tools
cargo run -p pi-cli -- --list-sessions

# 健康检查
cargo run -p pi-cli -- doctor
```

## 质量门禁

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench -p pi-bench --no-run
```

## 设计哲学

- **本地、中文优先**：默认中文输出，零网络也可用（echo + ollama）。
- **能力为先**：所有写文件、执行命令、网络、扩展 hostcall 必须经过策略决策。
- **证据胜过宣告**：性能数字必须能用 `cargo bench` 复现，对齐情况在 `docs/evidence/parity-ledger.md` 备案。
- **小核心 + 宽边界**：`pi-core` 不依赖任何本地 crate；产品逻辑住在边界 crate 里，方便剪裁。
