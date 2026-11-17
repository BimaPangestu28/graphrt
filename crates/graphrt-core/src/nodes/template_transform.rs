//! `template-transform`: renders a Jinja2-compatible template.
//!
//! Rendering is sandboxed: there is no filesystem loader, execution is bounded
//! by a fuel limit and a recursion limit, the output size is capped and
//! auto-escaping is off. Secret values are masked in the template context.

use futures::future::BoxFuture;
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult};
use crate::pool::Selector;
use crate::value::Value;

/// Instruction budget per render.
pub const FUEL: u64 = 100_000;
/// Maximum rendered output size in bytes.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const RECURSION_LIMIT: usize = 64;

/// `template-transform` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TemplateTransformConfig {
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub variables: Vec<TemplateVariable>,
}

/// A template variable bound to a selector.
#[derive(Clone, Debug, Deserialize)]
pub struct TemplateVariable {
    pub variable: String,
    pub value_selector: Selector,
}

/// The `template-transform` node.
#[derive(Debug)]
pub struct TemplateTransformNode {
    config: TemplateTransformConfig,
    env: Environment<'static>,
}

fn sandboxed_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_fuel(Some(FUEL));
    env.set_recursion_limit(RECURSION_LIMIT);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env
}

impl TemplateTransformNode {
    pub fn new(doc: &NodeDoc, config: TemplateTransformConfig) -> Result<Self, ConfigError> {
        let env = sandboxed_env();
        if let Err(e) = env.template_from_str(&config.template) {
            return Err(ConfigError::new(doc, format!("invalid template: {e}")));
        }
        Ok(Self { config, env })
    }
}

impl Node for TemplateTransformNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut vars = indexmap::IndexMap::with_capacity(self.config.variables.len());
            for v in &self.config.variables {
                vars.insert(v.variable.clone(), ctx.get(&v.value_selector).unwrap_or(Value::Null));
            }
            let context = minijinja::Value::from(minijinja::value::Serde(&vars));
            let output = self
                .env
                .render_str(&self.config.template, context)
                .map_err(|e| NodeError::fatal("template", e.to_string()))?;
            if output.len() > MAX_OUTPUT_BYTES {
                return Err(NodeError::fatal(
                    "template",
                    format!("output is {} bytes, limit is {MAX_OUTPUT_BYTES}", output.len()),
                ));
            }
            Ok(NodeOutput::new().with("output", output).into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        self.config.variables.iter().map(|v| v.value_selector.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use crate::pool::VariablePool;
    use serde_json::json;
    use std::sync::Arc;

    fn node(cfg: serde_json::Value) -> Result<TemplateTransformNode, ConfigError> {
        let doc = NodeDoc::new("t", "template-transform", cfg);
        TemplateTransformNode::new(&doc, doc.parse_config().unwrap())
    }

    async fn render(cfg: serde_json::Value, pool: VariablePool) -> NodeResult {
        node(cfg).unwrap().run(NodeContext::for_test("t", pool)).await
    }

    #[tokio::test]
    async fn renders_jinja() {
        let mut pool = VariablePool::new();
        pool.insert("n", "items", Value::from_json(json!(["a", "b"])));
        pool.insert("n", "key", Value::Secret(Arc::from("sk-live")));
        let r = render(
            json!({
                "template": "{% for i in items %}{{ i | upper }}{% endfor %} {{ key }} {{ missing }}|",
                "variables": [
                    {"variable": "items", "value_selector": ["n", "items"]},
                    {"variable": "key", "value_selector": ["n", "key"]}
                ]
            }),
            pool,
        )
        .await
        .unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("output"), Some(&Value::from("AB ****** |")));
    }

    #[test]
    fn syntax_error_is_config_error() {
        assert!(node(json!({"template": "{% for %}"})).is_err());
    }

    #[tokio::test]
    async fn runaway_template_is_stopped() {
        let r = render(
            json!({"template": "{% for i in range(100000) %}{% for j in range(100000) %}x{% endfor %}{% endfor %}"}),
            VariablePool::new(),
        )
        .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn no_filesystem_access() {
        let r = render(json!({"template": "{% include '/etc/passwd' %}"}), VariablePool::new()).await;
        assert!(r.is_err());
    }
}
