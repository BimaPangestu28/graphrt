//! [`TextModel`] for any [`ChatModel`]: one system prompt, one user message,
//! the answer as text. Used by work outside a run, such as drafting a skill
//! or suggesting lessons (cargo feature `text-model`).

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::text_model::{TextModel, TextModelError, TextRequest};

use crate::types::{ChatModel, ChatRequest, FinishReason, Message, collect};

/// A chat model used for plain completions.
#[derive(Clone, Debug)]
pub struct ChatTextModel {
    model: Arc<dyn ChatModel>,
    name: String,
}

impl ChatTextModel {
    /// Completions from `model`, asking the provider for model `name`.
    pub fn new(model: Arc<dyn ChatModel>, name: impl Into<String>) -> Self {
        Self { model, name: name.into() }
    }
}

impl TextModel for ChatTextModel {
    fn complete(&self, request: TextRequest) -> BoxFuture<'_, Result<String, TextModelError>> {
        Box::pin(async move {
            let mut chat =
                ChatRequest::new(self.name.clone(), vec![Message::system(request.system), Message::user(request.user)]);
            chat.max_tokens = Some(request.max_tokens);
            let stream = self.model.chat(chat).await.map_err(|error| TextModelError(error.to_string()))?;
            let response = collect(stream, |_| {}).await.map_err(|error| TextModelError(error.to_string()))?;
            if response.finish_reason == Some(FinishReason::Length) {
                return Err(TextModelError("the model's answer was cut off by the token limit".to_owned()));
            }
            if response.text.trim().is_empty() {
                return Err(TextModelError("the model returned no text".to_owned()));
            }
            Ok(response.text)
        })
    }
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::mock::MockModel;

    fn request() -> TextRequest {
        TextRequest { system: "Be brief.".into(), user: "Say hi.".into(), max_tokens: 50 }
    }

    #[tokio::test]
    async fn sends_one_system_and_one_user_message_and_returns_the_text() {
        let mock = Arc::new(MockModel::new(vec![MockModel::text("hi")]));
        let model = ChatTextModel::new(Arc::clone(&mock) as Arc<dyn ChatModel>, "gpt-test");
        assert_eq!(model.complete(request()).await, Ok("hi".to_owned()));
        let sent = mock.requests().pop().expect("one request");
        assert_eq!((sent.model.as_str(), sent.max_tokens, sent.messages.len()), ("gpt-test", Some(50), 2));
        assert!(sent.tools.is_empty(), "a completion offers no tools");
    }

    #[tokio::test]
    async fn an_empty_answer_is_an_error() {
        let mock = Arc::new(MockModel::new(vec![MockModel::text("  ")]));
        let model = ChatTextModel::new(mock as Arc<dyn ChatModel>, "gpt-test");
        assert!(model.complete(request()).await.is_err());
    }
}
