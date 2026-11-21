//! Live provider matrix: one tiny, cheap request per capability against the real
//! OpenAI, Anthropic, Gemini, AWS Bedrock and TypeSafe APIs.
//!
//! Opt-in twice, so a plain `cargo test` never spends money or needs network:
//!
//! 1. `GRAPHRT_LIVE_PROVIDERS=1` must be set, and
//! 2. the provider's own credentials must be present; a provider without them
//!    prints a skip line and passes, so forks and PRs without secrets stay green.
//!
//! | Provider | Credentials | Default model (override) |
//! |---|---|---|
//! | OpenAI | `OPENAI_API_KEY` | `gpt-4o-mini` (`GRAPHRT_LIVE_OPENAI_MODEL`) |
//! | Anthropic | `ANTHROPIC_API_KEY` | `claude-haiku-4-5` (`GRAPHRT_LIVE_ANTHROPIC_MODEL`) |
//! | Gemini | `GEMINI_API_KEY` | `gemini-2.5-flash-lite` (`GRAPHRT_LIVE_GEMINI_MODEL`) |
//! | Bedrock | `AWS_BEARER_TOKEN_BEDROCK`, or `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`); `AWS_REGION` (default `us-east-1`) | `us.amazon.nova-micro-v1:0` (`GRAPHRT_LIVE_BEDROCK_MODEL`) |
//! | TypeSafe | `TYPESAFE_API_KEY` (+ optional `TYPESAFE_BASE_URL`) | `jev-latest` (`GRAPHRT_LIVE_TYPESAFE_MODEL`) |
//!
//! Chat providers run two tests each: `<provider>::chat_streams_text_live` (a
//! streamed answer whose deltas add up to the final text, with usage) and
//! `<provider>::tool_call_round_trip_live` (a forced tool call with JSON arguments,
//! then the tool result sent back and answered): three requests of at most 48
//! output tokens per provider. TypeSafe is a decision API, not a chat model:
//! `typesafe::answers_typed_questions_live` asks one yes/no and one choice question
//! in a single request.
//!
//! ```sh
//! GRAPHRT_LIVE_PROVIDERS=1 OPENAI_API_KEY=... \
//!   cargo test -p graphrt-llm --features gemini,bedrock --test live_providers -- --nocapture openai::
//! ```
//!
//! Credential values are never printed: errors are scrubbed of every credential
//! the test read before they reach a panic message. Assertions check structure
//! (and one obvious fact per answer), not exact wording.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::fmt::Display;

/// Environment variable that switches the live tests on.
const LIVE_SWITCH: &str = "GRAPHRT_LIVE_PROVIDERS";
/// Output cap of every chat request: enough for one short sentence or one tool call.
const MAX_TOKENS: u32 = 48;

/// Variables of one provider read from the environment, or `None` (after
/// printing why) when the provider must be skipped.
///
/// `required` lists alternatives: the provider runs when every variable of at
/// least one alternative is set. `optional` variables are passed on when present.
fn provider_vars(provider: &str, required: &[&[&str]], optional: &[&str]) -> Option<Vec<(String, String)>> {
    if std::env::var(LIVE_SWITCH).ok().as_deref() != Some("1") {
        println!("skipping live {provider} test (set {LIVE_SWITCH}=1 and the provider's credentials to run)");
        return None;
    }
    let read =
        |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty()).map(|value| (name.to_owned(), value));
    let Some(alternative) = required.iter().find(|names| names.iter().all(|name| read(name).is_some())) else {
        let wanted: Vec<String> = required.iter().map(|names| names.join(" + ")).collect();
        println!("skipping live {provider} test: credentials not set (needs {})", wanted.join(" or "));
        return None;
    };
    Some(alternative.iter().chain(optional).filter_map(|name| read(name)).collect())
}

/// Model name: `GRAPHRT_LIVE_<PROVIDER>_MODEL` or the cheap default.
fn model_name(provider: &str, default: &str) -> String {
    let variable = format!("GRAPHRT_LIVE_{}_MODEL", provider.to_uppercase());
    std::env::var(variable).ok().filter(|name| !name.is_empty()).unwrap_or_else(|| default.to_owned())
}

/// Panic with `error`, scrubbed of every credential value in `vars`.
fn fail(context: &str, error: impl Display, vars: &[(String, String)]) -> ! {
    let secrets: Vec<&str> = vars.iter().map(|(_, value)| value.as_str()).collect();
    let message = error.to_string();
    panic!("{context}: {}", graphrt_core::redact::redact_secrets(&message, &secrets));
}

#[cfg(any(feature = "openai", feature = "anthropic", feature = "gemini", feature = "bedrock"))]
mod chat {
    use std::sync::Arc;
    use std::time::Instant;

    use graphrt_core::tool::ToolSpec;
    use graphrt_llm::{
        ChatDelta, ChatModel, ChatRequest, ChatResponse, FinishReason, Message, ModelRegistry, ToolChoice, collect,
    };
    use serde_json::json;

    use super::{MAX_TOKENS, fail};

    /// A provider under test: its registry name, model and the variables it was built from.
    pub struct LiveModel {
        pub provider: &'static str,
        pub model_name: String,
        pub model: Arc<dyn ChatModel>,
        pub vars: Vec<(String, String)>,
    }

    impl LiveModel {
        /// Build the provider from exactly `vars` (nothing else from the environment).
        pub fn new(provider: &'static str, model_name: String, vars: Vec<(String, String)>) -> Self {
            let registry = ModelRegistry::from_vars(&vars).unwrap_or_else(|e| fail(provider, e, &vars));
            let model = registry.get(provider).unwrap_or_else(|e| fail(provider, e, &vars));
            Self { provider, model_name, model, vars }
        }

        async fn send(&self, request: ChatRequest, mut on_text: impl FnMut(String)) -> ChatResponse {
            let stream = self.model.chat(request).await.unwrap_or_else(|e| fail(self.provider, e, &self.vars));
            let on_delta = |delta: ChatDelta| {
                if let ChatDelta::Text(text) = delta {
                    on_text(text);
                }
            };
            collect(stream, on_delta).await.unwrap_or_else(|e| fail(self.provider, e, &self.vars))
        }

        fn request(&self, messages: Vec<Message>) -> ChatRequest {
            ChatRequest::new(self.model_name.clone(), messages).with_max_tokens(MAX_TOKENS).with_temperature(0.0)
        }
    }

    /// One short streamed answer: deltas arrive, add up to the final text, and usage is reported.
    pub async fn chat_streams_text(live: &LiveModel) {
        let request = live.request(vec![
            Message::system("Answer with one short sentence."),
            Message::user("What color is the sky on a clear day?"),
        ]);
        let started = Instant::now();
        let mut first_delta = None;
        let mut deltas = Vec::new();
        let response = live
            .send(request, |text| {
                first_delta.get_or_insert_with(|| started.elapsed());
                deltas.push(text);
            })
            .await;
        println!(
            "[{}] {} text deltas, first after {:?}, total {:?}, usage {:?}",
            live.provider,
            deltas.len(),
            first_delta.unwrap_or_default(),
            started.elapsed(),
            response.usage
        );
        assert!(!deltas.is_empty(), "[{}] no text deltas were streamed", live.provider);
        assert_eq!(deltas.concat(), response.text, "[{}] streamed deltas must equal the final text", live.provider);
        assert!(
            response.text.to_lowercase().contains("blue"),
            "[{}] unexpected answer: {}",
            live.provider,
            response.text
        );
        assert!(response.usage.completion_tokens > 0, "[{}] usage missing: {:?}", live.provider, response.usage);
        assert!(
            matches!(response.finish_reason, Some(FinishReason::Stop | FinishReason::Length)),
            "[{}] finish reason {:?}",
            live.provider,
            response.finish_reason
        );
    }

    fn weather_tool() -> ToolSpec {
        ToolSpec {
            name: "get_weather".into(),
            description: "Current weather for a city.".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        }
    }

    /// A forced tool call with JSON arguments, then the tool result sent back and answered.
    pub async fn tool_call_round_trip(live: &LiveModel) {
        let question = Message::user("What is the weather in Oslo right now?");
        let request = live
            .request(vec![question.clone()])
            .with_tools(vec![weather_tool()])
            .with_tool_choice(ToolChoice::Tool("get_weather".into()));
        let call_turn = live.send(request, |_| {}).await;
        let [call] = call_turn.tool_calls.as_slice() else {
            panic!("[{}] expected exactly one tool call, got {:?}", live.provider, call_turn.tool_calls);
        };
        assert_eq!(call.name, "get_weather", "[{}]", live.provider);
        assert!(!call.id.is_empty(), "[{}] tool call without an id", live.provider);
        let arguments = call.arguments_json().unwrap_or_else(|e| panic!("[{}] {e}", live.provider));
        let city = arguments.get("city").and_then(|v| v.as_str()).unwrap_or_default();
        assert!(city.to_lowercase().contains("oslo"), "[{}] tool arguments: {}", live.provider, call.arguments);

        let messages = vec![
            question,
            call_turn.to_message(),
            Message::tool(call.id.clone(), "Oslo: 4 degrees Celsius, clear sky."),
        ];
        let request = live.request(messages).with_tools(vec![weather_tool()]).with_tool_choice(ToolChoice::None);
        let answer = live.send(request, |_| {}).await;
        println!(
            "[{}] tool call {}({}); answer: {} (usage {:?}, {:?})",
            live.provider, call.name, call.arguments, answer.text, call_turn.usage, answer.usage
        );
        assert!(
            answer.tool_calls.is_empty(),
            "[{}] unexpected second tool call: {:?}",
            live.provider,
            answer.tool_calls
        );
        assert!(answer.text.contains('4'), "[{}] answer should use the tool result: {}", live.provider, answer.text);
    }
}

/// Define `<provider>_chat_streams_text` and `<provider>_tool_call_round_trip`.
macro_rules! live_chat_tests {
    ($feature:literal, $provider:ident, $model:expr, required: $required:expr, optional: $optional:expr $(, defaults: $defaults:expr)?) => {
        #[cfg(feature = $feature)]
        mod $provider {
            use super::chat::{LiveModel, chat_streams_text, tool_call_round_trip};

            fn live_model() -> Option<LiveModel> {
                let provider = stringify!($provider);
                #[allow(unused_mut)]
                let mut vars = super::provider_vars(provider, $required, $optional)?;
                $(
                    for (name, value) in $defaults {
                        if !vars.iter().any(|(existing, _)| existing == name) {
                            vars.push((name.to_owned(), value.to_owned()));
                        }
                    }
                )?
                Some(LiveModel::new(provider, super::model_name(provider, $model), vars))
            }

            #[tokio::test]
            async fn chat_streams_text_live() {
                if let Some(live) = live_model() {
                    chat_streams_text(&live).await;
                }
            }

            #[tokio::test]
            async fn tool_call_round_trip_live() {
                if let Some(live) = live_model() {
                    tool_call_round_trip(&live).await;
                }
            }
        }
    };
}

live_chat_tests!("openai", openai, "gpt-4o-mini", required: &[&["OPENAI_API_KEY"]], optional: &["OPENAI_BASE_URL"]);
live_chat_tests!("anthropic", anthropic, "claude-haiku-4-5", required: &[&["ANTHROPIC_API_KEY"]], optional: &["ANTHROPIC_BASE_URL"]);
live_chat_tests!("gemini", gemini, "gemini-2.5-flash-lite", required: &[&["GEMINI_API_KEY"]], optional: &["GEMINI_BASE_URL"]);
live_chat_tests!(
    "bedrock",
    bedrock,
    "us.amazon.nova-micro-v1:0",
    required: &[&["AWS_BEARER_TOKEN_BEDROCK"], &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]],
    optional: &["AWS_SESSION_TOKEN", "AWS_REGION", "BEDROCK_ENDPOINT_URL"],
    defaults: [("AWS_REGION", "us-east-1")]
);

#[cfg(feature = "typesafe")]
mod typesafe {
    use graphrt_llm::typesafe::{Answer, Question, SystemOneClient, SystemOneRequest};
    use indexmap::IndexMap;
    use serde_json::json;

    use super::fail;

    /// One yes/no and one choice question about a billing message.
    #[tokio::test]
    async fn answers_typed_questions_live() {
        let Some(vars) = super::provider_vars("typesafe", &[&["TYPESAFE_API_KEY"]], &["TYPESAFE_BASE_URL"]) else {
            return;
        };
        let client = SystemOneClient::from_vars(&vars)
            .unwrap_or_else(|e| fail("typesafe", e, &vars))
            .unwrap_or_else(|| fail("typesafe", "TYPESAFE_API_KEY was read but no client was built", &vars));
        let mut topics = IndexMap::new();
        topics.insert("billing".to_owned(), json!("Billing, charges and payments"));
        topics.insert("bug".to_owned(), json!("A software bug or crash"));
        let request = SystemOneRequest::new(
            super::model_name("typesafe", "jev-latest"),
            json!("My card was charged twice for the same order."),
        )
        .question("is_billing", Question::noul("Is this message about billing or payments?"))
        .question("topic", Question::choice("What is this message about?", topics));
        let response = client.ask(&request).await.unwrap_or_else(|e| fail("typesafe", e, &vars));
        println!("[typesafe] model {}, answers {:?}, usage {:?}", response.model, response.answers, response.usage);
        match response.answer("is_billing") {
            Some(Answer::Noul { noul }) => assert!(*noul > 0.5, "[typesafe] noul {noul}"),
            other => panic!("[typesafe] expected a noul answer, got {other:?}"),
        }
        match response.answer("topic") {
            Some(Answer::Choice { choice, .. }) => assert_eq!(choice, "billing"),
            other => panic!("[typesafe] expected a choice answer, got {other:?}"),
        }
    }
}
