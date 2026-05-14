//! Slash-command framework.
//!
//! Commands run in-agent (no provider call) and return a short text the agent
//! can route to the consumer. We deliberately make them simple data so the TUI
//! and SDK consumers can render their own affordances.
//!
//! Custom commands: `<workspace>/.pi/commands/*.md` files are exposed as
//! `/<filename-without-ext>`. The file body becomes the assistant response,
//! after a simple `{{argument}}` substitution with the rest of the command line.

use std::fs;
use std::path::Path;

use pi_core::Event;

#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct SlashOutcome {
    pub events: Vec<Event>,
    pub assistant: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SlashRegistry {
    commands: Vec<SlashCommand>,
    custom: Vec<CustomCommand>,
}

#[derive(Debug, Clone)]
struct CustomCommand {
    name: String,
    template: String,
}

impl SlashRegistry {
    pub fn builtin() -> Self {
        Self {
            custom: Vec::new(),
            commands: vec![
                SlashCommand {
                    name: "/help",
                    description: "显示可用命令",
                },
                SlashCommand {
                    name: "/clear",
                    description: "清空当前会话",
                },
                SlashCommand {
                    name: "/sessions",
                    description: "列出本机会话",
                },
                SlashCommand {
                    name: "/model",
                    description: "切换模型 (语法：/model provider model)",
                },
                SlashCommand {
                    name: "/tools",
                    description: "列出可用工具",
                },
                SlashCommand {
                    name: "/compact",
                    description: "立即对当前上下文进行压缩",
                },
                SlashCommand {
                    name: "/permission",
                    description: "切换权限模式 (read-only/confirm/trusted/plan)",
                },
                SlashCommand {
                    name: "/quit",
                    description: "退出 pi",
                },
                SlashCommand {
                    name: "/init",
                    description: "在当前目录初始化 .pi/ 骨架（skills/commands/agents/prompts/resources/hooks）",
                },
            ],
        }
    }

    pub fn list(&self) -> Vec<&SlashCommand> {
        self.commands.iter().collect()
    }

    pub fn load_custom(&mut self, workspace_root: &Path) {
        let dir = workspace_root.join(".pi").join("commands");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|e| e.to_str()) else {
                continue;
            };
            let template = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(_) => continue,
            };
            self.custom.push(CustomCommand {
                name: format!("/{stem}"),
                template,
            });
        }
    }

    pub fn handle(&self, prompt: &str) -> Option<SlashOutcome> {
        let trimmed = prompt.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let (cmd, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        if let Some(custom) = self.custom.iter().find(|c| c.name == cmd) {
            let rendered = custom.template.replace("{{argument}}", rest);
            return Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some(rendered),
            });
        }
        match cmd {
            "/help" => {
                let mut text = String::from("可用命令：\n");
                for command in &self.commands {
                    text.push_str(&format!("- {}\t{}\n", command.name, command.description));
                }
                Some(SlashOutcome {
                    events: Vec::new(),
                    assistant: Some(text),
                })
            }
            "/tools" => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some("请通过 `pi --list-tools` 查看完整 schema。".to_string()),
            }),
            "/clear" => Some(SlashOutcome {
                events: vec![Event::SessionSaved {
                    path: "(cleared)".to_string(),
                }],
                assistant: Some("会话已标记为待清空（需在会话存储端执行）。".to_string()),
            }),
            "/sessions" => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some("使用 `pi --list-sessions` 查看本机会话。".to_string()),
            }),
            "/compact" => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some("已请求下一轮调用前执行上下文压缩。".to_string()),
            }),
            "/quit" => Some(SlashOutcome {
                events: vec![Event::Cancelled],
                assistant: Some("收到退出请求。".to_string()),
            }),
            "/model" => {
                if rest.trim().is_empty() {
                    Some(SlashOutcome {
                        events: Vec::new(),
                        assistant: Some("用法：/model <provider> <model>".to_string()),
                    })
                } else {
                    Some(SlashOutcome {
                        events: Vec::new(),
                        assistant: Some(format!(
                            "模型切换由前端处理；请重新启动 pi 并加上 `--provider/--model {rest}`",
                        )),
                    })
                }
            }
            "/permission" => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some(format!(
                    "用法：/permission <read-only|confirm|trusted|plan>；参数 {rest}",
                )),
            }),
            "/init" => {
                let report = init_workspace_skeleton();
                Some(SlashOutcome {
                    events: Vec::new(),
                    assistant: Some(report),
                })
            }
            other => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some(format!("未知命令：{other}。试试 /help。")),
            }),
        }
    }
}

/// Materialize the standard `.pi/` workspace skeleton in the current
/// directory. Each subdirectory is created idempotently; if a file is
/// missing we write a small placeholder so the user has something to edit.
/// Returns a human-readable summary the slash command can echo back.
fn init_workspace_skeleton() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return "无法读取当前目录，跳过 /init。".to_string();
    };
    let root = cwd.join(".pi");
    let subdirs = [
        "skills",
        "commands",
        "agents",
        "prompts",
        "resources",
        "hooks",
        "todos",
    ];
    let mut created: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for sub in subdirs {
        let dir = root.join(sub);
        if dir.exists() {
            skipped.push(sub.to_string());
        } else if fs::create_dir_all(&dir).is_ok() {
            created.push(sub.to_string());
        }
    }
    // .gitignore template — keeps sessions + todos out of version control by
    // default. The user can edit it after.
    let gitignore = root.join(".gitignore");
    if !gitignore.exists() {
        let _ = fs::write(
            &gitignore,
            "todos/\nsessions/\nauth.enc\n*.local\n",
        );
        created.push(".gitignore".to_string());
    }
    // Example system.md so users discover the workspace override.
    let system_md = root.join("system.md");
    if !system_md.exists() {
        let _ = fs::write(
            &system_md,
            "# Workspace System Prompt\n\n\
             pi 将这段内容作为系统提示词的开头（动态尾巴仍由 pi 自动注入）。\n\
             删除或留空这个文件来恢复默认提示词。\n",
        );
        created.push("system.md".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("✓ 工作区骨架已就绪：{}\n", root.display()));
    if !created.is_empty() {
        out.push_str(&format!("  新建：{}\n", created.join(", ")));
    }
    if !skipped.is_empty() {
        out.push_str(&format!("  已存在：{}\n", skipped.join(", ")));
    }
    out.push_str(
        "提示：现在可以把 markdown skill / 命令 / agent profile / prompt / 资源 / 钩子放到对应子目录里，pi 会在下一轮自动加载。",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_known_commands() {
        let registry = SlashRegistry::builtin();
        let outcome = registry.handle("/help").expect("outcome");
        let text = outcome.assistant.expect("text");
        assert!(text.contains("/help"));
        assert!(text.contains("/clear"));
    }

    #[test]
    fn unknown_command_yields_guidance() {
        let registry = SlashRegistry::builtin();
        let outcome = registry.handle("/wat").expect("outcome");
        assert!(outcome.assistant.unwrap().contains("未知命令"));
    }

    #[test]
    fn non_slash_prompts_pass_through() {
        let registry = SlashRegistry::builtin();
        assert!(registry.handle("hello").is_none());
    }

    #[test]
    fn init_creates_pi_skeleton_in_cwd() {
        // /init writes against the current working directory, so we have to
        // chdir into a tempdir for the duration of the test. Serialize with
        // a module-local mutex so parallel tests do not stomp each other.
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = TEST_CWD.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir.path()).expect("cd");
        let registry = SlashRegistry::builtin();
        let outcome = registry.handle("/init").expect("outcome");
        std::env::set_current_dir(prev).expect("restore cwd");
        let text = outcome.assistant.expect("text");
        assert!(text.contains("工作区骨架已就绪"));
        for sub in ["skills", "commands", "agents", "prompts", "resources", "hooks"] {
            let path = dir.path().join(".pi").join(sub);
            assert!(path.is_dir(), "missing dir: {}", path.display());
        }
        assert!(dir.path().join(".pi").join("system.md").is_file());
        assert!(dir.path().join(".pi").join(".gitignore").is_file());
    }

    use std::sync::Mutex;
    static TEST_CWD: Mutex<()> = Mutex::new(());
}
