//! GenAI semantic-convention span model: operation names, tool and retrieval
//! spans, content capture on and off, masking and caller context.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::engine::RunInput;
use graphrt_core::node::{CaptureContent, NodeResult, Registry};
use graphrt_core::{Engine, Graph, Node, NodeContext, NodeOutput, Services, Value};
use graphrt_otel::semconv::{self, gen_ai, langfuse};
use graphrt_otel::{AttrValue, CaptureMode, EventTracer, InMemoryExporter, RunContext, Span, StatusCode};
use serde_json::json;

/// Stands in for the `llm` node: reports rendered messages when asked to.
#[derive(Debug)]
struct FakeLlm;

impl Node for FakeLlm {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            if ctx.captures_content() {
                let key = ctx.get(&graphrt_core::Selector::of("start", "key")).unwrap_or(Value::Null);
                let prompt = format!("use {}", key.to_text());
                ctx.progress(
                    "model_request",
                    Value::from_json(json!({"messages": [
                        {"role": "system", "content": [{"type": "text", "text": "be brief"}]},
                        {"role": "user", "content": [{"type": "text", "text": prompt}]},
                    ]})),
                );
            }
            Ok(NodeOutput::new()
                .with("text", "the answer")
                .with("finish_reason", "stop")
                .usage(graphrt_core::Usage { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 })
                .meta("model", "gpt-test")
                .meta("provider", "openai")
                .into())
        })
    }
}

/// Stands in for the `agent` node: two tool calls, the second fails.
#[derive(Debug)]
struct FakeAgent;

impl Node for FakeAgent {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            ctx.progress("agent_step", Value::from_json(json!({"iteration": 1})));
            for (id, name) in [("c1", "lookup"), ("c2", "explode")] {
                ctx.progress("tool_call", Value::from_json(json!({"id": id, "name": name, "arguments": {"q": "x"}})));
            }
            let mut ok = json!({"id": "c1", "name": "lookup", "is_error": false, "images": 0});
            let mut bad = json!({"id": "c2", "name": "explode", "is_error": true, "images": 0});
            if ctx.captures_content() {
                ok["result"] = json!("found it");
                bad["result"] = json!("error: boom");
            }
            ctx.progress("tool_result", Value::from_json(ok));
            ctx.progress("tool_result", Value::from_json(bad));
            Ok(NodeOutput::new().with("text", "done").into())
        })
    }
}

/// Stands in for `knowledge-retrieval`.
#[derive(Debug)]
struct FakeRetrieval;

impl Node for FakeRetrieval {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let hit = Value::from_json(json!({"id": "chunk-1", "document_id": "doc-1", "score": 0.9, "content": "c"}));
            let mut out = NodeOutput::new().with("result", Value::array([hit]));
            if ctx.captures_content() {
                out = out.meta("query", "what is x");
            }
            Ok(out.into())
        })
    }
}

#[derive(Debug)]
struct FakeSystemOne;

impl Node for FakeSystemOne {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().with("score", 0.5).into()) })
    }
}

fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    r.register("llm", |_| Ok(Arc::new(FakeLlm)));
    r.register("agent", |_| Ok(Arc::new(FakeAgent)));
    r.register("knowledge-retrieval", |_| Ok(Arc::new(FakeRetrieval)));
    r.register("system-one", |_| Ok(Arc::new(FakeSystemOne)));
    r
}

const RAG: &str = r#"
name: rag
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: question, type: string }, { variable: key, type: secret }] }
    - { id: kb, type: knowledge-retrieval, title: Search, dataset_ids: [docs], multiple_retrieval_config: { top_k: 3 } }
    - { id: answer, type: llm, title: Answer, model: { provider: openai, name: gpt-test } }
    - { id: helper, type: agent, title: Helper, model: { provider: anthropic, name: claude-test } }
    - { id: judge, type: system-one, title: Judge }
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [answer, text] }] }
  edges:
    - { source: start, target: kb }
    - { source: kb, target: answer }
    - { source: answer, target: helper }
    - { source: helper, target: judge }
    - { source: judge, target: end }
"#;

async fn trace(mode: CaptureMode, context: RunContext) -> Vec<Span> {
    let graph = Arc::new(Graph::from_text(RAG, &registry()).unwrap());
    let exporter = InMemoryExporter::new();
    let mut tracer =
        EventTracer::new(Arc::new(exporter.clone())).with_graph(graph.clone()).capture(mode).with_context(context);
    let mut services = Services::new();
    if mode.is_enabled() {
        services.insert(CaptureContent);
    }
    let engine = Engine::new(graph).with_services(services);
    let input = RunInput::new().input("question", "what is x").input("key", "sk-live-123456");
    let mut run = engine.run(input);
    while let Some(event) = run.next().await {
        tracer.on_event(&event);
    }
    drop(tracer);
    exporter.take()
}

fn by_name<'a>(spans: &'a [Span], name: &str) -> &'a Span {
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    spans.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no span `{name}` in {names:?}"))
}

fn str_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    match span.attr(key) {
        Some(AttrValue::Str(s)) => Some(s),
        _ => None,
    }
}

fn structured<'a>(span: &'a Span, key: &str) -> &'a serde_json::Value {
    match span.attr(key) {
        Some(AttrValue::Structured(v)) => v,
        other => panic!("`{key}` is not structured on `{}`: {other:?}", span.name),
    }
}

const CONTENT_KEYS: &[&str] = &[
    gen_ai::INPUT_MESSAGES,
    gen_ai::OUTPUT_MESSAGES,
    gen_ai::SYSTEM_INSTRUCTIONS,
    gen_ai::TOOL_CALL_ARGUMENTS,
    gen_ai::TOOL_CALL_RESULT,
    gen_ai::RETRIEVAL_QUERY_TEXT,
    gen_ai::RETRIEVAL_DOCUMENTS,
    langfuse::OBSERVATION_INPUT,
    langfuse::OBSERVATION_OUTPUT,
    semconv::graphrt::RUN_INPUT,
    semconv::graphrt::RUN_OUTPUT,
    "graphrt.node.outputs",
];

#[tokio::test]
async fn spans_follow_the_genai_conventions() {
    let spans = trace(CaptureMode::NoContent, RunContext::default()).await;
    let root = by_name(&spans, "invoke_workflow rag");
    assert!(root.parent_span_id.is_none());
    assert_eq!(str_attr(root, gen_ai::OPERATION_NAME), Some("invoke_workflow"));
    assert_eq!(str_attr(root, gen_ai::AGENT_NAME), Some("rag"));
    assert!(root.is_run_root());

    let chat = by_name(&spans, "chat gpt-test");
    assert_eq!(chat.parent_span_id, Some(root.span_id));
    assert_eq!(str_attr(chat, gen_ai::OPERATION_NAME), Some("chat"));
    assert_eq!(str_attr(chat, gen_ai::PROVIDER_NAME), Some("openai"));
    assert_eq!(str_attr(chat, gen_ai::SYSTEM), Some("openai"), "deprecated name dual-emitted");
    assert_eq!(str_attr(chat, gen_ai::REQUEST_MODEL), Some("gpt-test"));
    assert_eq!(str_attr(chat, semconv::graphrt::NODE_TITLE), Some("Answer"));
    assert_eq!(chat.attr(gen_ai::USAGE_INPUT_TOKENS), Some(&AttrValue::Int(5)));
    assert_eq!(structured(chat, gen_ai::RESPONSE_FINISH_REASONS), &json!(["stop"]));

    let agent = by_name(&spans, "invoke_agent Helper");
    assert_eq!(str_attr(agent, gen_ai::OPERATION_NAME), Some("invoke_agent"));
    assert_eq!(str_attr(agent, gen_ai::AGENT_NAME), Some("Helper"));
    assert_eq!(str_attr(agent, gen_ai::PROVIDER_NAME), Some("anthropic"));
    assert!(agent.events.iter().any(|e| e.name == "agent_step"));
    assert!(agent.events.iter().all(|e| e.name != "tool_call" && e.name != "tool_result"));

    let lookup = by_name(&spans, "execute_tool lookup");
    assert_eq!(lookup.parent_span_id, Some(agent.span_id));
    assert_eq!(lookup.trace_id, root.trace_id);
    assert_eq!(str_attr(lookup, gen_ai::OPERATION_NAME), Some("execute_tool"));
    assert_eq!(str_attr(lookup, gen_ai::TOOL_NAME), Some("lookup"));
    assert_eq!(str_attr(lookup, gen_ai::TOOL_CALL_ID), Some("c1"));
    assert_eq!(str_attr(lookup, gen_ai::AGENT_NAME), Some("Helper"));
    assert_eq!(lookup.status, StatusCode::Unset);
    let explode = by_name(&spans, "execute_tool explode");
    assert_eq!(explode.status, StatusCode::Error);
    assert_eq!(str_attr(explode, "error.type"), Some("tool_error"));

    let retrieval = by_name(&spans, "retrieval");
    assert_eq!(str_attr(retrieval, gen_ai::OPERATION_NAME), Some("retrieval"));
    assert_eq!(retrieval.attr(gen_ai::RETRIEVAL_TOP_K), Some(&AttrValue::Int(3)));
    assert_eq!(str_attr(retrieval, gen_ai::DATA_SOURCE_ID), Some("docs"));

    let judge = by_name(&spans, "chat");
    assert_eq!(str_attr(judge, gen_ai::OPERATION_NAME), Some("chat"));
    assert_eq!(str_attr(judge, semconv::graphrt::OPERATION_KIND), Some("classify"));
    assert_eq!(str_attr(judge, gen_ai::PROVIDER_NAME), Some("typesafe"));

    // Plain nodes keep their titles.
    assert!(spans.iter().any(|s| s.name == "start"));
    // Nothing content-bearing without opt-in.
    for span in &spans {
        for key in CONTENT_KEYS {
            assert!(span.attr(key).is_none(), "`{key}` on `{}` with capture off", span.name);
        }
        assert!(span.events.iter().all(|e| e.name != semconv::CONTENT_EVENT));
    }
}

#[tokio::test]
async fn span_capture_records_content_with_secrets_masked() {
    let spans = trace(CaptureMode::SpanOnly, RunContext::default()).await;
    let root = by_name(&spans, "invoke_workflow rag");
    let input = str_attr(root, semconv::graphrt::RUN_INPUT).unwrap();
    assert!(input.contains("what is x") && input.contains("******"), "{input}");
    assert!(!input.contains("sk-live-123456"), "secret leaked: {input}");
    assert_eq!(str_attr(root, langfuse::OBSERVATION_INPUT), Some(input));
    let output = str_attr(root, langfuse::OBSERVATION_OUTPUT).unwrap();
    assert!(output.contains("the answer"), "{output}");
    assert_eq!(str_attr(root, semconv::graphrt::RUN_OUTPUT), Some(output));

    let chat = by_name(&spans, "chat gpt-test");
    assert_eq!(structured(chat, gen_ai::SYSTEM_INSTRUCTIONS), &json!([{"type": "text", "content": "be brief"}]));
    let messages = structured(chat, gen_ai::INPUT_MESSAGES);
    assert_eq!(messages[0]["role"], "user");
    let prompt = messages[0]["parts"][0]["content"].as_str().unwrap();
    assert_eq!(prompt, "use ******", "secrets in rendered prompts stay masked");
    let output = structured(chat, gen_ai::OUTPUT_MESSAGES);
    assert_eq!(output[0]["parts"][0]["content"], "the answer");
    assert_eq!(output[0]["finish_reason"], "stop");
    assert_eq!(str_attr(chat, langfuse::OBSERVATION_OUTPUT), Some("the answer"));

    let lookup = by_name(&spans, "execute_tool lookup");
    assert_eq!(str_attr(lookup, gen_ai::TOOL_CALL_ARGUMENTS), Some(r#"{"q":"x"}"#));
    assert_eq!(str_attr(lookup, gen_ai::TOOL_CALL_RESULT), Some("found it"));
    assert_eq!(str_attr(by_name(&spans, "execute_tool explode"), gen_ai::TOOL_CALL_RESULT), Some("error: boom"));

    let retrieval = by_name(&spans, "retrieval");
    assert_eq!(str_attr(retrieval, gen_ai::RETRIEVAL_QUERY_TEXT), Some("what is x"));
    assert_eq!(structured(retrieval, gen_ai::RETRIEVAL_DOCUMENTS), &json!([{"id": "chunk-1", "score": 0.9}]));

    for span in &spans {
        assert!(span.events.iter().all(|e| e.name != semconv::CONTENT_EVENT), "SPAN_ONLY adds no content events");
    }
}

#[tokio::test]
async fn event_capture_moves_content_to_span_events() {
    let spans = trace(CaptureMode::EventOnly, RunContext::default()).await;
    let chat = by_name(&spans, "chat gpt-test");
    assert!(chat.attr(gen_ai::INPUT_MESSAGES).is_none());
    assert!(chat.attr(langfuse::OBSERVATION_INPUT).is_none());
    let event = chat.events.iter().find(|e| e.name == semconv::CONTENT_EVENT).unwrap();
    let keys: Vec<&str> = event.attributes.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&gen_ai::INPUT_MESSAGES) && keys.contains(&gen_ai::OUTPUT_MESSAGES), "{keys:?}");
    let root = by_name(&spans, "invoke_workflow rag");
    let event = root.events.iter().find(|e| e.name == semconv::CONTENT_EVENT).unwrap();
    assert!(event.attributes.iter().any(|(k, _)| k == semconv::graphrt::RUN_INPUT));

    let both = trace(CaptureMode::SpanAndEvent, RunContext::default()).await;
    let chat = by_name(&both, "chat gpt-test");
    assert!(chat.attr(gen_ai::INPUT_MESSAGES).is_some());
    assert!(chat.events.iter().any(|e| e.name == semconv::CONTENT_EVENT));
}

#[tokio::test]
async fn caller_context_is_stamped_on_every_span() {
    let context = RunContext {
        session_id: Some("sess-1".into()),
        user_id: Some("user-7".into()),
        tags: vec!["prod".into(), "beta".into()],
        metadata: vec![("tenant".into(), "acme".into())],
    };
    let spans = trace(CaptureMode::NoContent, context).await;
    for span in &spans {
        assert_eq!(str_attr(span, "session.id"), Some("sess-1"), "{}", span.name);
        assert_eq!(str_attr(span, langfuse::SESSION_ID), Some("sess-1"));
        assert_eq!(str_attr(span, "user.id"), Some("user-7"));
        assert_eq!(str_attr(span, langfuse::USER_ID), Some("user-7"));
        assert_eq!(structured(span, langfuse::TRACE_TAGS), &json!(["prod", "beta"]));
        assert_eq!(str_attr(span, "langfuse.trace.metadata.tenant"), Some("acme"));
        let genai = span.attr(gen_ai::OPERATION_NAME).is_some();
        assert_eq!(str_attr(span, gen_ai::CONVERSATION_ID).is_some(), genai, "{}", span.name);
    }
    let root = by_name(&spans, "invoke_workflow rag");
    assert_eq!(str_attr(root, "graphrt.run.metadata.tenant"), Some("acme"));
    assert_eq!(structured(root, semconv::graphrt::RUN_TAGS), &json!(["prod", "beta"]));
}
