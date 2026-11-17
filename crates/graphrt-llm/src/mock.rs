//! Scripted model for tests and offline examples.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use futures::future::BoxFuture;
use graphrt_core::event::Usage;

use crate::error::LlmError;
use crate::types::{ChatDelta, ChatModel, ChatRequest, ChatStream, FinishReason};

/// A model that replays scripted responses in order and records every request.
#[derive(Debug, Default)]
pub struct MockModel {
    script: Mutex<VecDeque<Result<Vec<ChatDelta>, LlmError>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MockModel {
    /// One entry per expected `chat` call.
    pub fn new(responses: Vec<Vec<ChatDelta>>) -> Self {
        Self { script: Mutex::new(responses.into_iter().map(Ok).collect()), requests: Mutex::default() }
    }

    /// Script a response consisting of plain text.
    pub fn text(text: &str) -> Vec<ChatDelta> {
        vec![
            ChatDelta::Text(text.to_owned()),
            ChatDelta::Usage(Usage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }),
            ChatDelta::Finish(FinishReason::Stop),
        ]
    }

    /// Script a response containing tool calls `(id, name, arguments)`.
    pub fn tool_calls(calls: &[(&str, &str, &str)]) -> Vec<ChatDelta> {
        let mut out: Vec<ChatDelta> = calls
            .iter()
            .enumerate()
            .map(|(index, (id, name, args))| ChatDelta::ToolCall {
                index,
                id: Some((*id).to_owned()),
                name: Some((*name).to_owned()),
                arguments: (*args).to_owned(),
            })
            .collect();
        out.push(ChatDelta::Usage(Usage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }));
        out.push(ChatDelta::Finish(FinishReason::ToolCalls));
        out
    }

    /// Append a scripted response.
    pub fn push(&self, deltas: Vec<ChatDelta>) {
        lock(&self.script).push_back(Ok(deltas));
    }

    /// Append a scripted error.
    pub fn push_error(&self, error: LlmError) {
        lock(&self.script).push_back(Err(error));
    }

    /// Requests received so far.
    pub fn requests(&self) -> Vec<ChatRequest> {
        lock(&self.requests).clone()
    }
}

impl ChatModel for MockModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        lock(&self.requests).push(request);
        let next = lock(&self.script).pop_front();
        Box::pin(async move {
            let deltas = next.unwrap_or_else(|| Err(LlmError::Config("mock model script exhausted".into())))?;
            let stream: ChatStream = Box::pin(futures::stream::iter(deltas.into_iter().map(Ok)));
            Ok(stream)
        })
    }
}
