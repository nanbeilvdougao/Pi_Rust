use pi_core::{AppConfig, Event, Message, PiResult, Role};
use pi_permissions::{PermissionEngine, PermissionMode};
use pi_providers::{provider_for, ProviderRequest};
use pi_session::{Session, SessionStore};
use pi_tools::{ToolCall, ToolRuntime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurn {
    pub session: Session,
    pub events: Vec<Event>,
}

pub struct AgentRuntime<S: SessionStore> {
    config: AppConfig,
    session_store: S,
    tools: ToolRuntime,
    permissions: PermissionEngine,
}

impl<S: SessionStore> AgentRuntime<S> {
    pub fn new(config: AppConfig, session_store: S) -> Self {
        Self {
            config,
            session_store,
            tools: ToolRuntime::builtin(),
            permissions: PermissionEngine::new(PermissionMode::ConfirmMutations),
        }
    }

    pub fn try_new(config: AppConfig, session_store: S) -> PiResult<Self> {
        let tools = match &config.enabled_tool_names {
            Some(names) => ToolRuntime::builtin_with_names(names)?,
            None => ToolRuntime::builtin(),
        };

        Ok(Self {
            config,
            session_store,
            tools,
            permissions: PermissionEngine::new(PermissionMode::ConfirmMutations),
        })
    }

    pub fn run_single_turn(&mut self, session_id: &str, prompt: &str) -> PiResult<AgentTurn> {
        let mut events = vec![Event::UserMessage(prompt.to_string())];
        let mut session = self.session_store.load(session_id)?;
        let user_message = Message::new(Role::User, prompt);
        self.session_store.append(session_id, &user_message)?;
        session.push(user_message);

        if self.config.tools_enabled {
            if let Some(call) = parse_tool_shortcut(prompt) {
                events.push(Event::ToolStarted {
                    name: call.name.clone(),
                });
                let output = self.tools.run(call, &mut self.permissions)?;
                events.push(Event::ToolFinished {
                    name: output.name.clone(),
                    output: output.output.clone(),
                });
                let tool_message = Message::new(Role::Tool, output.output);
                self.session_store.append(session_id, &tool_message)?;
                session.push(tool_message);
                return Ok(AgentTurn { session, events });
            }
        }

        let provider = provider_for(&self.config.model)?;
        let response = provider.complete(ProviderRequest {
            model: self.config.model.clone(),
            messages: session.messages.clone(),
        })?;

        for delta in response.events {
            events.push(Event::AssistantDelta(delta));
        }
        events.push(Event::AssistantMessage(response.message.content.clone()));
        self.session_store.append(session_id, &response.message)?;
        session.push(response.message);

        Ok(AgentTurn { session, events })
    }

    pub fn tool_schemas(&self) -> Vec<pi_core::ToolSchema> {
        self.tools.schemas()
    }
}

fn parse_tool_shortcut(prompt: &str) -> Option<ToolCall> {
    let rest = prompt.strip_prefix("/tool ")?;
    let (name, input) = rest.split_once(' ')?;
    Some(ToolCall {
        name: name.to_string(),
        input: input.to_string(),
    })
}
