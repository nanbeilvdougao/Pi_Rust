//! Localized hints + actionable next steps for `PiError`.
//!
//! TS pi has `error-hints.ts` that maps every error class to a "what should
//! the user actually do" paragraph. We mirror that, with both zh-CN and en
//! variants. The hint never replaces the underlying error message — it just
//! gets appended when callers want a friendlier UX (e.g. CLI/TUI display
//! path; SDK consumers still get the raw error).
//!
//! Adding a new hint:
//!   - Match on `PiErrorKind` plus optional keyword in the error message.
//!   - Keep zh-CN as the primary; English is parallel.

use crate::{Locale, PiError, PiErrorKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorHint {
    pub summary: String,
    pub suggestions: Vec<String>,
}

impl ErrorHint {
    pub fn format(&self) -> String {
        let mut out = self.summary.clone();
        for suggestion in &self.suggestions {
            out.push_str("\n  · ");
            out.push_str(suggestion);
        }
        out
    }
}

pub fn hint_for(error: &PiError, locale: Locale) -> ErrorHint {
    let zh = matches!(locale, Locale::ZhCn);
    let message = &error.message;
    match error.kind {
        PiErrorKind::Provider
            if message.contains("缺少凭证") || message.contains("缺少环境变量") =>
        {
            if zh {
                ErrorHint {
                    summary: "缺少 provider 凭证。".to_string(),
                    suggestions: vec![
                        "运行 `pi auth set <provider>` 写入加密存储；".to_string(),
                        "或在 shell 中 export 对应的环境变量；".to_string(),
                        "用 `pi --list-providers` 查看每个 provider 需要哪个变量。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Missing provider credential.".to_string(),
                    suggestions: vec![
                        "Run `pi auth set <provider>` to store it encrypted.".to_string(),
                        "Or export the corresponding env var in your shell.".to_string(),
                        "`pi --list-providers` shows the env var per provider.".to_string(),
                    ],
                }
            }
        }
        PiErrorKind::Provider
            if message.contains("HTTP 401") || message.contains("Unauthorized") =>
        {
            if zh {
                ErrorHint {
                    summary: "Provider 拒绝了凭证。".to_string(),
                    suggestions: vec![
                        "确认 API key 未过期或被吊销；".to_string(),
                        "运行 `pi auth set <provider>` 重新写入；".to_string(),
                        "确认 `pi --provider` 与 key 对应（不要把 OpenAI key 给 Anthropic）。"
                            .to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Provider rejected the credential.".to_string(),
                    suggestions: vec![
                        "Check that the key has not been rotated or revoked.".to_string(),
                        "Re-run `pi auth set <provider>` to refresh.".to_string(),
                        "Confirm provider matches the key (do not mix OpenAI vs. Anthropic keys)."
                            .to_string(),
                    ],
                }
            }
        }
        PiErrorKind::Provider if message.contains("HTTP 429") => {
            if zh {
                ErrorHint {
                    summary: "Provider 速率限制。".to_string(),
                    suggestions: vec![
                        "稍等几秒后重试；".to_string(),
                        "降低 `--max-steps` 或 `compaction_threshold` 减少 token；".to_string(),
                        "切换到 ollama 本地模型避免远端配额。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Provider rate-limited.".to_string(),
                    suggestions: vec![
                        "Wait a few seconds and retry.".to_string(),
                        "Reduce `--max-steps` or `compaction_threshold` to use fewer tokens."
                            .to_string(),
                        "Switch to `--provider ollama` for local inference.".to_string(),
                    ],
                }
            }
        }
        PiErrorKind::Network => {
            if zh {
                ErrorHint {
                    summary: "网络访问失败。".to_string(),
                    suggestions: vec![
                        "检查代理、防火墙、DNS；".to_string(),
                        "确认 provider 的 base URL（如 `OLLAMA_BASE_URL`、`OPENAI_BASE_URL`）正确；".to_string(),
                        "运行 `pi doctor` 查看 curl 是否可用。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Network call failed.".to_string(),
                    suggestions: vec![
                        "Check proxy, firewall, and DNS.".to_string(),
                        "Confirm provider base URL (`OLLAMA_BASE_URL`, `OPENAI_BASE_URL`)."
                            .to_string(),
                        "Run `pi doctor` for transport diagnostics.".to_string(),
                    ],
                }
            }
        }
        PiErrorKind::PermissionDenied => {
            if zh {
                ErrorHint {
                    summary: "操作被权限策略拒绝。".to_string(),
                    suggestions: vec![
                        "用 `--permission trusted` 提升模式（仅在受信工作区使用）；".to_string(),
                        "在 `.pi/config.toml` 中设 `permission_mode`；".to_string(),
                        "如果是 sandbox 阻挡，设置 `SandboxProfile::workspace_root` 把目标加入白名单。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Operation denied by permission policy.".to_string(),
                    suggestions: vec![
                        "Use `--permission trusted` (only inside a trusted workspace).".to_string(),
                        "Set `permission_mode` in `.pi/config.toml`.".to_string(),
                        "If sandbox blocked it, extend `SandboxProfile::workspace_root`."
                            .to_string(),
                    ],
                }
            }
        }
        PiErrorKind::Session => {
            if zh {
                ErrorHint {
                    summary: "会话存储出错。".to_string(),
                    suggestions: vec![
                        "确认 `~/.pi-rust/sessions/` 可写；".to_string(),
                        "用 `pi --list-sessions` 查看现有会话；".to_string(),
                        "考虑 `--export-json <id>` 备份后再 `--delete-session`。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Session store error.".to_string(),
                    suggestions: vec![
                        "Check that `~/.pi-rust/sessions/` is writable.".to_string(),
                        "Use `pi --list-sessions` to inspect existing sessions.".to_string(),
                        "Back up via `--export-json <id>` before `--delete-session`.".to_string(),
                    ],
                }
            }
        }
        PiErrorKind::NotFound => {
            if zh {
                ErrorHint {
                    summary: "目标不存在。".to_string(),
                    suggestions: vec![
                        "确认会话 / 文件名拼写；".to_string(),
                        "运行 `pi --list-sessions`/`ls` 验证目标存在。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Target not found.".to_string(),
                    suggestions: vec![
                        "Double-check the spelling.".to_string(),
                        "Run `pi --list-sessions`/`ls` to verify it exists.".to_string(),
                    ],
                }
            }
        }
        PiErrorKind::Cancelled => {
            if zh {
                ErrorHint {
                    summary: "操作已取消。".to_string(),
                    suggestions: vec!["可以重新提问或调整提示词后再发。".to_string()],
                }
            } else {
                ErrorHint {
                    summary: "Operation cancelled.".to_string(),
                    suggestions: vec!["Re-send the prompt or adjust it.".to_string()],
                }
            }
        }
        PiErrorKind::InvalidInput => {
            if zh {
                ErrorHint {
                    summary: "输入参数无效。".to_string(),
                    suggestions: vec![
                        "运行 `pi --help` 查看完整用法；".to_string(),
                        "如果是 JSON 工具输入，确认是合法对象（缩进、引号成对）。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Invalid input.".to_string(),
                    suggestions: vec![
                        "Run `pi --help` for the canonical usage.".to_string(),
                        "If you fed a tool a JSON object, verify quoting/braces.".to_string(),
                    ],
                }
            }
        }
        _ => {
            if zh {
                ErrorHint {
                    summary: "运行出错。".to_string(),
                    suggestions: vec![
                        "尝试 `pi doctor` 检查环境；".to_string(),
                        "在 issue 中附上 `pi --version` 与完整错误信息。".to_string(),
                    ],
                }
            } else {
                ErrorHint {
                    summary: "Runtime error.".to_string(),
                    suggestions: vec![
                        "Run `pi doctor` to inspect the environment.".to_string(),
                        "Open an issue with `pi --version` and the full error.".to_string(),
                    ],
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PiError, PiErrorKind};

    #[test]
    fn missing_credential_hint_mentions_auth_set() {
        let err = PiError::new(PiErrorKind::Provider, "缺少凭证 OPENAI_API_KEY");
        let hint = hint_for(&err, Locale::ZhCn);
        assert!(hint.suggestions.iter().any(|s| s.contains("pi auth set")));
    }

    #[test]
    fn rate_limit_hint_suggests_local() {
        let err = PiError::new(PiErrorKind::Provider, "HTTP 429: rate limited");
        let hint = hint_for(&err, Locale::En);
        assert!(hint
            .suggestions
            .iter()
            .any(|s| s.to_lowercase().contains("ollama")));
    }

    #[test]
    fn locale_switches_language() {
        let err = PiError::new(PiErrorKind::Network, "connection refused");
        let zh = hint_for(&err, Locale::ZhCn);
        let en = hint_for(&err, Locale::En);
        assert!(zh.summary.contains("网络"));
        assert!(en.summary.contains("Network"));
    }

    #[test]
    fn format_renders_suggestion_bullets() {
        let hint = ErrorHint {
            summary: "x".to_string(),
            suggestions: vec!["a".to_string(), "b".to_string()],
        };
        let rendered = hint.format();
        assert!(rendered.contains("· a"));
        assert!(rendered.contains("· b"));
    }
}
