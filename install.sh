#!/usr/bin/env sh
set -eu

if ! command -v cargo >/dev/null 2>&1; then
  echo "未找到 cargo。请先安装 Rust 工具链：https://rustup.rs" >&2
  exit 1
fi

cargo install --path crates/pi-cli --bin pi

echo "Pi Rust 已安装。运行：pi --help"
