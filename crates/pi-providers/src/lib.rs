use std::collections::BTreeMap;

use pi_core::{Message, ModelSelection, PiError, PiErrorKind, PiResult, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub default_model: String,
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
        registry.register(ProviderInfo {
            id: "ollama".to_string(),
            display_name: "Ollama 本地模型".to_string(),
            default_model: "qwen2.5:7b".to_string(),
            local_first: true,
            requires_api_key_env: None,
        });
        registry.register(ProviderInfo {
            id: "moonshot".to_string(),
            display_name: "Moonshot 月之暗面".to_string(),
            default_model: "moonshot-v1-8k".to_string(),
            local_first: false,
            requires_api_key_env: Some("MOONSHOT_API_KEY".to_string()),
        });
        registry.register(ProviderInfo {
            id: "deepseek".to_string(),
            display_name: "DeepSeek".to_string(),
            default_model: "deepseek-chat".to_string(),
            local_first: false,
            requires_api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
        });
        registry.register(ProviderInfo {
            id: "qwen".to_string(),
            display_name: "通义千问".to_string(),
            default_model: "qwen-plus".to_string(),
            local_first: false,
            requires_api_key_env: Some("DASHSCOPE_API_KEY".to_string()),
        });
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
        other => Err(PiError::new(
            PiErrorKind::Provider,
            format!(
                "provider `{other}` 已注册为目标能力，但 MVP 目前只实现本地 echo 执行路径"
            ),
        )),
    }
}
