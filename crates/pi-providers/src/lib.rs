use std::collections::BTreeMap;
use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use pi_core::{escape_json_string, Message, ModelSelection, PiError, PiErrorKind, PiResult, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub default_model: String,
    pub supported_models: Vec<String>,
    pub local_first: bool,
    pub requires_api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    pub model: ModelSelection,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderResponse {
    pub message: Message,
    pub events: Vec<String>,
}

pub trait Provider {
    fn info(&self) -> ProviderInfo;
    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse>;
}

#[derive(Debug, Default)]
pub struct EchoProvider;

impl Provider for EchoProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "echo".to_string(),
            display_name: "本地 Echo Provider".to_string(),
            default_model: "echo-local".to_string(),
            supported_models: vec!["echo-local".to_string()],
            local_first: true,
            requires_api_key_env: None,
        }
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .unwrap_or("");

        let content = format!(
            "这是 Pi Rust MVP 的本地响应。已收到你的请求：{last_user}"
        );

        Ok(ProviderResponse {
            message: Message::new(Role::Assistant, content.clone()),
            events: vec![content],
        })
    }
}

#[derive(Debug, Default)]
pub struct OllamaProvider;

impl Provider for OllamaProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "ollama".to_string(),
            display_name: "Ollama 本地模型".to_string(),
            default_model: "qwen2.5:7b".to_string(),
            supported_models: vec![
                "qwen2.5:7b".to_string(),
                "qwen2.5:3b".to_string(),
                "llama3.1:8b".to_string(),
                "deepseek-r1:7b".to_string(),
            ],
            local_first: true,
            requires_api_key_env: None,
        }
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let base_url = env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
        let endpoint = HttpEndpoint::parse(&base_url)?;
        let body = ollama_chat_body(&request);
        let response = post_json(&endpoint, "/api/chat", &body)?;
        let content = extract_ollama_message_content(&response).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                "Ollama 响应中缺少 message.content 字段",
            )
        })?;

        Ok(ProviderResponse {
            message: Message::new(Role::Assistant, content.clone()),
            events: vec![content],
        })
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    info: ProviderInfo,
    api_key_env: &'static str,
    endpoint: &'static str,
}

impl OpenAiCompatibleProvider {
    pub fn moonshot() -> Self {
        Self {
            info: moonshot_info(),
            api_key_env: "MOONSHOT_API_KEY",
            endpoint: "https://api.moonshot.cn/v1/chat/completions",
        }
    }

    pub fn deepseek() -> Self {
        Self {
            info: deepseek_info(),
            api_key_env: "DEEPSEEK_API_KEY",
            endpoint: "https://api.deepseek.com/chat/completions",
        }
    }

    pub fn qwen() -> Self {
        Self {
            info: qwen_info(),
            api_key_env: "DASHSCOPE_API_KEY",
            endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        }
    }
}

impl Provider for OpenAiCompatibleProvider {
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let api_key = env::var(self.api_key_env).map_err(|_| {
            PiError::new(
                PiErrorKind::Provider,
                format!("缺少环境变量 {}，无法调用 {}", self.api_key_env, self.info.id),
            )
        })?;
        let body = openai_chat_body(&request);
        let response = post_openai_compatible(self.endpoint, &api_key, &body)?;
        let content = extract_openai_message_content(&response).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                format!("{} 响应中缺少 choices[0].message.content 字段", self.info.id),
            )
        })?;

        Ok(ProviderResponse {
            message: Message::new(Role::Assistant, content.clone()),
            events: vec![content],
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderInfo>,
}

impl ProviderRegistry {
    pub fn builtin() -> Self {
        let mut registry = Self {
            providers: BTreeMap::new(),
        };
        registry.register(EchoProvider.info());
        registry.register(ollama_info());
        registry.register(moonshot_info());
        registry.register(deepseek_info());
        registry.register(qwen_info());
        registry
    }

    pub fn register(&mut self, info: ProviderInfo) {
        self.providers.insert(info.id.clone(), info);
    }

    pub fn list(&self) -> impl Iterator<Item = &ProviderInfo> {
        self.providers.values()
    }

    pub fn get(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.get(id)
    }

    pub fn require(&self, id: &str) -> PiResult<&ProviderInfo> {
        self.get(id).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                format!("未知 provider：{id}。请运行 `pi --list-providers` 查看可用项。"),
            )
        })
    }
}

pub fn provider_for(selection: &ModelSelection) -> PiResult<Box<dyn Provider>> {
    match selection.provider.as_str() {
        "echo" => Ok(Box::new(EchoProvider)),
        "ollama" => Ok(Box::new(OllamaProvider)),
        "moonshot" => Ok(Box::new(OpenAiCompatibleProvider::moonshot())),
        "deepseek" => Ok(Box::new(OpenAiCompatibleProvider::deepseek())),
        "qwen" => Ok(Box::new(OpenAiCompatibleProvider::qwen())),
        other => Err(PiError::new(
            PiErrorKind::Provider,
            format!(
                "provider `{other}` 已注册为目标能力，但 MVP 目前只实现本地 echo 执行路径"
            ),
        )),
    }
}

fn ollama_info() -> ProviderInfo {
    ProviderInfo {
        id: "ollama".to_string(),
        display_name: "Ollama 本地模型".to_string(),
        default_model: "qwen2.5:7b".to_string(),
        supported_models: vec![
            "qwen2.5:7b".to_string(),
            "qwen2.5:3b".to_string(),
            "llama3.1:8b".to_string(),
            "deepseek-r1:7b".to_string(),
        ],
        local_first: true,
        requires_api_key_env: None,
    }
}

fn moonshot_info() -> ProviderInfo {
    ProviderInfo {
        id: "moonshot".to_string(),
        display_name: "Moonshot 月之暗面".to_string(),
        default_model: "moonshot-v1-8k".to_string(),
        supported_models: vec!["moonshot-v1-8k".to_string(), "moonshot-v1-32k".to_string()],
        local_first: false,
        requires_api_key_env: Some("MOONSHOT_API_KEY".to_string()),
    }
}

fn deepseek_info() -> ProviderInfo {
    ProviderInfo {
        id: "deepseek".to_string(),
        display_name: "DeepSeek".to_string(),
        default_model: "deepseek-chat".to_string(),
        supported_models: vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()],
        local_first: false,
        requires_api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
    }
}

fn qwen_info() -> ProviderInfo {
    ProviderInfo {
        id: "qwen".to_string(),
        display_name: "通义千问".to_string(),
        default_model: "qwen-plus".to_string(),
        supported_models: vec!["qwen-plus".to_string(), "qwen-turbo".to_string()],
        local_first: false,
        requires_api_key_env: Some("DASHSCOPE_API_KEY".to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpEndpoint {
    host: String,
    port: u16,
}

impl HttpEndpoint {
    fn parse(base_url: &str) -> PiResult<Self> {
        let without_scheme = base_url.strip_prefix("http://").ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                "MVP Ollama provider 仅支持 http:// OLLAMA_BASE_URL",
            )
        })?;
        let authority = without_scheme.trim_end_matches('/');
        let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
            let parsed_port = port.parse::<u16>().map_err(|_| {
                PiError::new(PiErrorKind::Provider, "OLLAMA_BASE_URL 端口格式无效")
            })?;
            (host.to_string(), parsed_port)
        } else {
            (authority.to_string(), 80)
        };

        if host.is_empty() {
            return Err(PiError::new(
                PiErrorKind::Provider,
                "OLLAMA_BASE_URL 缺少主机名",
            ));
        }

        Ok(Self { host, port })
    }
}

fn ollama_chat_body(request: &ProviderRequest) -> String {
    let messages = request
        .messages
        .iter()
        .map(|message| {
            format!(
                "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                message.role.as_str(),
                escape_json_string(&message.content)
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "{{\"model\":\"{}\",\"stream\":false,\"messages\":[{}]}}",
        escape_json_string(&request.model.model),
        messages
    )
}

fn openai_chat_body(request: &ProviderRequest) -> String {
    let messages = request
        .messages
        .iter()
        .filter(|message| message.role != Role::Tool)
        .map(|message| {
            format!(
                "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                message.role.as_str(),
                escape_json_string(&message.content)
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "{{\"model\":\"{}\",\"stream\":false,\"messages\":[{}]}}",
        escape_json_string(&request.model.model),
        messages
    )
}

fn post_openai_compatible(endpoint: &str, api_key: &str, body: &str) -> PiResult<String> {
    let auth_header = format!("Authorization: Bearer {api_key}");
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            "120",
            "-X",
            "POST",
            endpoint,
            "-H",
            "Content-Type: application/json",
            "-H",
            auth_header.as_str(),
            "--data-binary",
            body,
        ])
        .output()
        .map_err(|err| {
            PiError::new(
                PiErrorKind::Provider,
                format!("无法启动 curl 调用 provider：{err}"),
            )
        })?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(PiError::new(
            PiErrorKind::Provider,
            format!("provider 请求失败：{stderr}{stdout}"),
        ))
    }
}

fn post_json(endpoint: &HttpEndpoint, path: &str, body: &str) -> PiResult<String> {
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("无法连接 Ollama：{err}。请确认 `ollama serve` 正在运行。"),
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        endpoint.host,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        PiError::new(PiErrorKind::Provider, "Ollama HTTP 响应格式无效")
    })?;

    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return Err(PiError::new(
            PiErrorKind::Provider,
            format!("Ollama 请求失败：{}", headers.lines().next().unwrap_or("未知状态")),
        ));
    }

    Ok(body.to_string())
}

fn extract_ollama_message_content(response: &str) -> Option<String> {
    let message_start = response.find("\"message\"")?;
    let content_part = &response[message_start..];
    extract_json_field(content_part, "content").map(unescape_json_string)
}

fn extract_openai_message_content(response: &str) -> Option<String> {
    let message_start = response.find("\"message\"")?;
    let content_part = &response[message_start..];
    extract_json_field(content_part, "content").map(unescape_json_string)
}

fn extract_json_field(input: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = input.find(&needle)? + needle.len();
    let rest = &input[start..];
    let mut out = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn unescape_json_string(input: String) -> String {
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_body_uses_chat_shape() {
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "ollama".to_string(),
                model: "qwen2.5:7b".to_string(),
            },
            messages: vec![Message::new(Role::User, "你好")],
        };

        let body = ollama_chat_body(&request);
        assert!(body.contains("\"model\":\"qwen2.5:7b\""));
        assert!(body.contains("\"stream\":false"));
        assert!(body.contains("\"role\":\"user\""));
        assert!(body.contains("你好"));
    }

    #[test]
    fn extracts_ollama_message_content() {
        let response = r#"{"message":{"role":"assistant","content":"你好，世界"}}"#;
        assert_eq!(
            extract_ollama_message_content(response),
            Some("你好，世界".to_string())
        );
    }

    #[test]
    fn openai_body_filters_tool_messages() {
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
            },
            messages: vec![
                Message::new(Role::User, "解释代码"),
                Message::new(Role::Tool, "tool output"),
            ],
        };

        let body = openai_chat_body(&request);
        assert!(body.contains("\"model\":\"deepseek-chat\""));
        assert!(body.contains("解释代码"));
        assert!(!body.contains("tool output"));
    }

    #[test]
    fn extracts_openai_message_content() {
        let response = r#"{"choices":[{"message":{"role":"assistant","content":"可以"}}]}"#;
        assert_eq!(
            extract_openai_message_content(response),
            Some("可以".to_string())
        );
    }
}
