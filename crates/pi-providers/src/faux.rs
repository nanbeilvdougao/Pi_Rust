//! Faux provider for tests, regression harnesses, and the SDK harness mode.
//!
//! A `FauxProvider` is constructed from an ordered script of turns. Each turn
//! is either a plain text reply, a sequence of stream events, a tool-call
//! request, or a typed error. The provider returns turns FIFO and records the
//! exact `ProviderRequest`s it received so tests can assert on them.
//!
//! Use `FauxProvider::with_script(...)` for one-shot setups; use
//! `FauxProvider::shared()` + `Arc<FauxProvider>` when the same instance must
//! be visible across the agent and the test.
//!
//! This is the rust equivalent of TS pi's `coding-agent/test/suite/harness.ts`
//! plus its faux provider.

use std::sync::{Arc, Mutex};

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};

use crate::{
    text_stream_events, tool_call_stream_events, Provider, ProviderInfo, ProviderRequest,
    ProviderResponse,
};

#[derive(Debug, Clone)]
pub enum FauxTurn {
    /// Reply with plain text. We synthesize the stream as a single TextDelta.
    Text(String),
    /// Reply with text split into N chunks so callers can verify streaming.
    Chunks(Vec<String>),
    /// Issue tool calls; the agent will run them and re-enter.
    ToolCalls(Vec<ToolInvocation>),
    /// Reply with a `(text, tool_calls)` pair (assistant turn carrying both).
    Mixed {
        text: String,
        tool_calls: Vec<ToolInvocation>,
    },
    /// Synthetic error — useful for negative-path tests.
    Error(PiError),
    /// Record usage tokens alongside the reply.
    Usage { text: String, usage: Usage },
}

#[derive(Debug, Default)]
pub struct FauxRecorder {
    pub requests: Vec<ProviderRequest>,
}

#[derive(Debug)]
pub struct FauxProvider {
    info: ProviderInfo,
    script: Mutex<std::collections::VecDeque<FauxTurn>>,
    recorder: Mutex<FauxRecorder>,
    arc_self: Mutex<Option<Arc<Self>>>,
}

impl FauxProvider {
    pub fn with_script(script: impl IntoIterator<Item = FauxTurn>) -> Arc<Self> {
        let provider = Arc::new(Self {
            info: ProviderInfo {
                id: "faux".to_string(),
                display_name: "Faux Provider".to_string(),
                default_model: "faux".to_string(),
                supported_models: vec!["faux".to_string()],
                local_first: true,
                requires_api_key_env: None,
            },
            script: Mutex::new(script.into_iter().collect()),
            recorder: Mutex::new(FauxRecorder::default()),
            arc_self: Mutex::new(None),
        });
        if let Ok(mut slot) = provider.arc_self.lock() {
            *slot = Some(provider.clone());
        }
        provider
    }

    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.recorder
            .lock()
            .map(|r| r.requests.clone())
            .unwrap_or_default()
    }

    pub fn push(&self, turn: FauxTurn) {
        if let Ok(mut script) = self.script.lock() {
            script.push_back(turn);
        }
    }

    fn next_turn(&self) -> FauxTurn {
        self.script
            .lock()
            .ok()
            .and_then(|mut script| script.pop_front())
            .unwrap_or_else(|| FauxTurn::Text("(faux: end of script)".to_string()))
    }

    fn render(&self, turn: FauxTurn) -> PiResult<ProviderResponse> {
        match turn {
            FauxTurn::Text(text) => Ok(text_response(text, Usage::default())),
            FauxTurn::Chunks(chunks) => Ok(chunks_response(chunks, Usage::default())),
            FauxTurn::ToolCalls(calls) => Ok(tool_calls_response(String::new(), calls)),
            FauxTurn::Mixed { text, tool_calls } => Ok(tool_calls_response(text, tool_calls)),
            FauxTurn::Error(err) => Err(err),
            FauxTurn::Usage { text, usage } => Ok(text_response(text, usage)),
        }
    }
}

fn text_response(content: String, usage: Usage) -> ProviderResponse {
    let stream = text_stream_events(&content);
    let events = if content.is_empty() {
        Vec::new()
    } else {
        vec![content.clone()]
    };
    ProviderResponse {
        message: Message::new(Role::Assistant, content),
        events,
        stream_events: stream,
        tool_calls: Vec::new(),
        usage,
    }
}

fn chunks_response(chunks: Vec<String>, usage: Usage) -> ProviderResponse {
    let mut stream = vec![StreamEvent::MessageStart];
    let mut joined = String::new();
    for chunk in &chunks {
        joined.push_str(chunk);
        stream.push(StreamEvent::TextDelta(chunk.clone()));
    }
    stream.push(StreamEvent::MessageDone);
    let events = if joined.is_empty() {
        Vec::new()
    } else {
        vec![joined.clone()]
    };
    ProviderResponse {
        message: Message::new(Role::Assistant, joined),
        events,
        stream_events: stream,
        tool_calls: Vec::new(),
        usage,
    }
}

fn tool_calls_response(text: String, calls: Vec<ToolInvocation>) -> ProviderResponse {
    let stream = if calls.is_empty() {
        text_stream_events(&text)
    } else {
        let mut s = vec![StreamEvent::MessageStart];
        if !text.is_empty() {
            s.push(StreamEvent::TextDelta(text.clone()));
        }
        s.extend(
            tool_call_stream_events(&calls)
                .into_iter()
                .filter(|e| !matches!(e, StreamEvent::MessageStart | StreamEvent::MessageDone)),
        );
        s.push(StreamEvent::MessageDone);
        s
    };
    let events = if text.is_empty() {
        Vec::new()
    } else {
        vec![text.clone()]
    };
    let mut message = Message::new(Role::Assistant, text);
    message.tool_calls = calls.clone();
    ProviderResponse {
        message,
        events,
        stream_events: stream,
        tool_calls: calls,
        usage: Usage::default(),
    }
}

impl Provider for FauxProvider {
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        if let Ok(mut rec) = self.recorder.lock() {
            rec.requests.push(request.clone());
        }
        let turn = self.next_turn();
        self.render(turn)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let response = self.complete(request)?;
        for event in &response.stream_events {
            if sink.cancelled() {
                sink.emit(StreamEvent::MessageDone)?;
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            sink.emit(event.clone())?;
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{ModelSelection, VecSink};

    fn req(text: &str) -> ProviderRequest {
        ProviderRequest::new(
            ModelSelection {
                provider: "faux".into(),
                model: "faux".into(),
            },
            vec![Message::new(Role::User, text)],
        )
    }

    #[test]
    fn returns_scripted_turns_in_order() {
        let provider = FauxProvider::with_script([
            FauxTurn::Text("first".into()),
            FauxTurn::Text("second".into()),
        ]);
        assert_eq!(
            provider.complete(req("a")).unwrap().message.content,
            "first"
        );
        assert_eq!(
            provider.complete(req("b")).unwrap().message.content,
            "second"
        );
    }

    #[test]
    fn records_received_requests() {
        let provider = FauxProvider::with_script([FauxTurn::Text("ok".into())]);
        provider.complete(req("hello")).unwrap();
        let recorded = provider.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].messages[0].content, "hello");
    }

    #[test]
    fn chunks_stream_individually() {
        let provider = FauxProvider::with_script([FauxTurn::Chunks(vec!["a".into(), "b".into()])]);
        let mut sink = VecSink::default();
        provider.stream(req("x"), &mut sink).unwrap();
        let deltas: Vec<_> = sink
            .events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["a", "b"]);
    }

    #[test]
    fn error_turn_propagates() {
        let provider = FauxProvider::with_script([FauxTurn::Error(PiError::new(
            PiErrorKind::Provider,
            "boom",
        ))]);
        let err = provider.complete(req("x")).unwrap_err();
        assert_eq!(err.message, "boom");
    }
}
