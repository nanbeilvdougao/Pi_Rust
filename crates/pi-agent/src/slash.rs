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
            other => Some(SlashOutcome {
                events: Vec::new(),
                assistant: Some(format!("未知命令：{other}。试试 /help。")),
            }),
        }
    }
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
}
