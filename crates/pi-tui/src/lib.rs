use pi_core::{Event, Message, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    InputChanged(String),
    Submit(String),
    Render(Event),
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    pub input: String,
    pub transcript: Vec<Message>,
    pub status: String,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            input: String::new(),
            transcript: Vec::new(),
            status: "就绪".to_string(),
        }
    }
}

impl TuiState {
    pub fn apply(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::InputChanged(input) => self.input = input,
            TuiEvent::Submit(prompt) => {
                self.transcript.push(Message::new(Role::User, prompt));
                self.input.clear();
                self.status = "等待模型响应".to_string();
            }
            TuiEvent::Render(Event::AssistantMessage(content)) => {
                self.transcript.push(Message::new(Role::Assistant, content));
                self.status = "就绪".to_string();
            }
            TuiEvent::Render(Event::ToolFinished { output, .. }) => {
                self.transcript.push(Message::new(Role::Tool, output));
                self.status = "工具完成".to_string();
            }
            TuiEvent::Render(_) => {}
            TuiEvent::Quit => self.status = "退出".to_string(),
        }
    }
}

pub fn render_transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| format!("{}: {}", message.role.as_str(), message.content))
        .collect::<Vec<_>>()
        .join("\n")
}
