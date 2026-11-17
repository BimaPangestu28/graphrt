//! `answer`: renders a text template into the user-facing reply.
//!
//! Output `answer` is the rendered text. Files referenced by the template
//! (file or `array[file]` values) are collected into `files`.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::node::{Node, NodeContext, NodeOutput, NodeResult, NodeRole};
use crate::pool::Selector;
use crate::template::Template;
use crate::value::Value;

/// `answer` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AnswerConfig {
    #[serde(default)]
    pub answer: Template,
}

/// The `answer` node.
#[derive(Debug)]
pub struct AnswerNode {
    config: AnswerConfig,
}

impl AnswerNode {
    pub fn new(config: AnswerConfig) -> Self {
        Self { config }
    }

    /// The parsed answer template (used for ordered response streaming).
    pub fn template(&self) -> &Template {
        &self.config.answer
    }
}

impl Node for AnswerNode {
    fn role(&self) -> NodeRole {
        NodeRole::Response
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let text = self.config.answer.render(&ctx.pool);
            let mut files = Vec::new();
            for selector in self.config.answer.selectors() {
                match ctx.get(selector) {
                    Some(f @ Value::File(_)) => files.push(f),
                    Some(Value::Array(items)) => {
                        files.extend(items.iter().filter(|i| matches!(i, Value::File(_))).cloned());
                    }
                    _ => {}
                }
            }
            let mut out = NodeOutput::new().with("answer", text);
            if !files.is_empty() {
                out = out.with("files", Value::array(files));
            }
            Ok(out.into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        self.config.answer.selectors().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use crate::pool::VariablePool;
    use crate::value::File;
    use std::sync::Arc;

    #[tokio::test]
    async fn renders_and_collects_files() {
        let cfg: AnswerConfig =
            serde_json::from_value(serde_json::json!({"answer": "Hi {{#llm.text#}} {{#n.f#}} {{#env.K#}}"})).unwrap();
        let mut pool = VariablePool::new();
        pool.insert("llm", "text", Value::from("there"));
        pool.insert("n", "f", Value::from(File { name: "a.png".into(), ..File::default() }));
        pool.insert("env", "K", Value::Secret(Arc::from("secret")));
        let NodeOutcome::Completed(o) = AnswerNode::new(cfg).run(NodeContext::for_test("ans", pool)).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(o.outputs.get("answer"), Some(&Value::from("Hi there a.png ******")));
        assert!(matches!(o.outputs.get("files"), Some(Value::Array(a)) if a.len() == 1));
    }
}
