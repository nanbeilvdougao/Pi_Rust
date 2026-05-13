# Pi Rust

Pi Rust 是一个从零设计的 Rust 版 Pi 终端 AI 编程助手。

本项目不以复刻现有 Rust 移植为目标，而是以更清晰的架构、更可信的兼容证据、更好的中文体验和更低维护成本为目标。

## 目标

- 与 TypeScript 版 Pi 的核心 CLI、工具、会话和 provider 行为保持可验证兼容。
- 默认采用能力权限模型，文件写入、命令执行、网络和扩展 hostcall 都必须经过策略决策。
- 优先支持中文用户场景，包括中文错误提示、国内模型提供商、Ollama、本地/离线安装和 openEuler/epkg 能力。
- 用契约测试、差异账本和可复现 benchmark 证明质量，而不是依赖口头性能声明。

## Workspace

- `crates/pi-core`：核心类型、事件、错误和配置模型。
- `crates/pi-agent`：Agent loop、工具调用、provider 调度和上下文压缩边界。
- `crates/pi-providers`：LLM provider 抽象和内置 provider 注册表。
- `crates/pi-tools`：内置工具系统。
- `crates/pi-permissions`：能力、审批、审计和沙箱策略。
- `crates/pi-session`：JSONL 会话存储和后续索引边界。
- `crates/pi-tui`：TUI 事件和渲染边界。
- `crates/pi-ext`：扩展 ABI、hostcall 和 conformance 边界。
- `crates/pi-cli`：最终 `pi` 命令入口。

## 当前状态

这是 MVP 骨架，已经包含基础 CLI、echo provider、JSONL 会话、权限引擎、内置文件工具和扩展 ABI 设计边界。

```bash
cargo run -p pi-cli -- --help
cargo run -p pi-cli -- -p "你好，介绍一下这个项目"
```

## 质量门禁

每个功能完成后应提交并推送到远端：

```bash
git add .
git commit -m "..."
git push
```

在发布前必须至少通过：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
