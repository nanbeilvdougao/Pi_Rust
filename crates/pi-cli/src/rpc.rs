//! JSON-RPC over stdio for SDK consumers.
//!
//! Wire format: line-delimited JSON. Each line is a single request, each
//! response a single line.
//!
//! Methods:
//! - `health` → `{"version":"…"}`
//! - `list_providers` → `[{id,display_name,…}]`
//! - `list_tools` → `[{name,description,parameters,mutates}]`
//! - `complete` → `{prompt, session_id?}` → `{events, session, usage}`
//!
//! Notes:
//! - Errors come back as `{"error": "…", "id": …}`.
//! - The RPC consumer is responsible for its own framing; we send one line
//!   per response and flush after each write.

use std::io::{self, BufRead, BufReader, Write};

use pi_agent::AgentRuntime;
use pi_core::{Event, PiResult};
use pi_providers::ProviderRegistry;
use pi_session::JsonlSessionStore;
use pi_tools::ToolRuntime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    id: &'a Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn run<R: BufRead, W: Write>(
    agent: &mut AgentRuntime<JsonlSessionStore>,
    reader: R,
    mut writer: W,
) -> PiResult<()> {
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                let resp = Response {
                    id: &None,
                    result: None,
                    error: Some(format!("读取请求失败：{err}")),
                };
                write_response(&mut writer, &resp)?;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let resp = Response {
                    id: &None,
                    result: None,
                    error: Some(format!("解析请求失败：{err}; line={line}")),
                };
                write_response(&mut writer, &resp)?;
                continue;
            }
        };
        let response = handle(&request, agent);
        write_response(&mut writer, &response)?;
        if request.method == "shutdown" {
            break;
        }
    }
    Ok(())
}

pub fn run_stdio(agent: &mut AgentRuntime<JsonlSessionStore>) -> PiResult<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let reader = BufReader::new(stdin.lock());
    let writer = stdout.lock();
    run(agent, reader, writer)
}

fn write_response<W: Write>(writer: &mut W, response: &Response<'_>) -> PiResult<()> {
    let line = serde_json::to_string(response)
        .unwrap_or_else(|_| "{\"error\":\"serialize failed\"}".to_string());
    writeln!(writer, "{line}")?;
    writer.flush()?;
    Ok(())
}

fn handle<'a>(request: &'a Request, agent: &mut AgentRuntime<JsonlSessionStore>) -> Response<'a> {
    match request.method.as_str() {
        "health" => Response {
            id: &request.id,
            result: Some(serde_json::json!({"version": pi_core::VERSION})),
            error: None,
        },
        "list_providers" => Response {
            id: &request.id,
            result: Some(serde_json::json!(ProviderRegistry::builtin()
                .list()
                .map(|p| serde_json::json!({
                    "id": p.id,
                    "display_name": p.display_name,
                    "default_model": p.default_model,
                    "supported_models": p.supported_models,
                    "local_first": p.local_first,
                    "requires_api_key_env": p.requires_api_key_env,
                }))
                .collect::<Vec<_>>())),
            error: None,
        },
        "list_tools" => Response {
            id: &request.id,
            result: Some(
                serde_json::to_value(ToolRuntime::builtin().schemas()).unwrap_or(Value::Null),
            ),
            error: None,
        },
        "list_models" => {
            // List every (provider, model) pair from the built-in registry.
            // Callers can also pass `{"provider": "openai"}` to filter.
            let filter = request
                .params
                .get("provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut pairs: Vec<Value> = Vec::new();
            for info in ProviderRegistry::builtin().list() {
                if filter.as_deref().map(|f| info.id != f).unwrap_or(false) {
                    continue;
                }
                for model in &info.supported_models {
                    pairs.push(serde_json::json!({
                        "provider": info.id,
                        "model": model,
                        "default": &info.default_model == model,
                    }));
                }
            }
            Response {
                id: &request.id,
                result: Some(Value::Array(pairs)),
                error: None,
            }
        }
        "list_aliases" => {
            let rows: Vec<Value> = pi_providers::aliases::aliases_table()
                .iter()
                .map(|(alias, provider, model)| {
                    serde_json::json!({
                        "alias": alias,
                        "provider": provider,
                        "model": model,
                    })
                })
                .collect();
            Response {
                id: &request.id,
                result: Some(Value::Array(rows)),
                error: None,
            }
        }
        "list_sessions" => match agent.session_store().list() {
            Ok(sessions) => Response {
                id: &request.id,
                result: Some(serde_json::to_value(sessions).unwrap_or(Value::Null)),
                error: None,
            },
            Err(err) => Response {
                id: &request.id,
                result: None,
                error: Some(err.to_string()),
            },
        },
        "get_config" => Response {
            id: &request.id,
            result: Some(serde_json::to_value(agent.config()).unwrap_or(Value::Null)),
            error: None,
        },
        "cancel" => {
            agent.cancel();
            Response {
                id: &request.id,
                result: Some(serde_json::json!({"cancelled": true})),
                error: None,
            }
        }
        "complete" => {
            let prompt = request
                .params
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let session_id = request
                .params
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            match agent.run_single_turn(&session_id, prompt) {
                Ok(turn) => Response {
                    id: &request.id,
                    result: Some(serde_json::json!({
                        "events": turn.events.iter().map(serialize_event).collect::<Vec<_>>(),
                        "session": turn.session,
                        "usage": turn.usage,
                    })),
                    error: None,
                },
                Err(err) => Response {
                    id: &request.id,
                    result: None,
                    error: Some(err.to_string()),
                },
            }
        }
        "shutdown" => Response {
            id: &request.id,
            result: Some(Value::Null),
            error: None,
        },
        other => Response {
            id: &request.id,
            result: None,
            error: Some(format!("unknown method: {other}")),
        },
    }
}

fn serialize_event(event: &Event) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{AppConfig, ModelSelection};
    use std::io::Cursor;

    fn temp_store() -> (tempfile::TempDir, JsonlSessionStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonlSessionStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn responds_to_health_check() {
        let (_dir, store) = temp_store();
        let mut agent = AgentRuntime::try_new(
            AppConfig {
                model: ModelSelection {
                    provider: "echo".into(),
                    model: "echo-local".into(),
                },
                tools_enabled: false,
                ..AppConfig::default()
            },
            store,
        )
        .expect("agent");
        let input = b"{\"id\":1,\"method\":\"health\"}\n{\"id\":2,\"method\":\"shutdown\"}\n";
        let reader = BufReader::new(Cursor::new(input.to_vec()));
        let mut output: Vec<u8> = Vec::new();
        run(&mut agent, reader, &mut output).expect("run");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("\"version\""));
        assert!(text.contains("\"id\":1"));
    }

    #[test]
    fn complete_returns_events_and_session() {
        let (_dir, store) = temp_store();
        let mut agent = AgentRuntime::try_new(
            AppConfig {
                model: ModelSelection {
                    provider: "echo".into(),
                    model: "echo-local".into(),
                },
                tools_enabled: false,
                ..AppConfig::default()
            },
            store,
        )
        .expect("agent");
        let input = b"{\"id\":3,\"method\":\"complete\",\"params\":{\"prompt\":\"hi\"}}\n{\"id\":4,\"method\":\"shutdown\"}\n";
        let reader = BufReader::new(Cursor::new(input.to_vec()));
        let mut output: Vec<u8> = Vec::new();
        run(&mut agent, reader, &mut output).expect("run");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("\"events\""));
        assert!(text.contains("\"session\""));
    }
}
