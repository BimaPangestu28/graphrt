//! `template-transform`: renders a Jinja2-compatible template.
//!
//! Rendering is sandboxed: there is no filesystem loader, execution is bounded
//! by a fuel limit and a recursion limit, the output size is capped and
//! auto-escaping is off. Secret values are masked in the template context.

use std::sync::Arc;

use futures::future::BoxFuture;
use minijinja::value::{Enumerator, Object};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult};
use crate::pool::{Selector, VariablePool};
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
    /// `config.variables`, shared with every render's [`TemplateVars`].
    variables: Arc<[TemplateVariable]>,
    env: Environment<'static>,
}

/// The template context of one render: each variable is resolved from the
/// node's pool snapshot when the template reads it.
///
/// This replaces building a map of every variable and converting it into a
/// minijinja map on each render, which cost several allocations per node
/// execution; now a render allocates only this object.
#[derive(Debug)]
struct TemplateVars {
    variables: Arc<[TemplateVariable]>,
    pool: VariablePool,
}

impl Object for TemplateVars {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        self.get_value_by_str(key.as_str()?)
    }

    fn get_value_by_str(self: &Arc<Self>, key: &str) -> Option<minijinja::Value> {
        // A repeated name binds its last declaration, as a map built in order would.
        let variable = self.variables.iter().rev().find(|v| v.variable == key)?;
        let value = self.pool.get(&variable.value_selector).unwrap_or(Value::Null);
        // Through `Serialize`, which masks secrets.
        Some(minijinja::Value::from(minijinja::value::Serde(&value)))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut names: Vec<&str> = Vec::with_capacity(self.variables.len());
        for v in self.variables.iter() {
            if !names.contains(&v.variable.as_str()) {
                names.push(&v.variable);
            }
        }
        Enumerator::Values(names.into_iter().map(minijinja::Value::from).collect())
    }
}

fn sandboxed_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_fuel(Some(FUEL));
    env.set_recursion_limit(RECURSION_LIMIT);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    env
}

/// Name of the node's only template inside its private environment.
const TEMPLATE_NAME: &str = "template";

impl TemplateTransformNode {
    /// Compile the template once; every run renders the compiled form instead of
    /// re-parsing the source (about 6x cheaper per render).
    pub fn new(doc: &NodeDoc, config: TemplateTransformConfig) -> Result<Self, ConfigError> {
        let mut env = sandboxed_env();
        if let Err(e) = env.add_template_owned(TEMPLATE_NAME, config.template.clone()) {
            return Err(ConfigError::new(doc, format!("invalid template: {e}")));
        }
        let variables = config.variables.clone().into();
        Ok(Self { config, variables, env })
    }
}

impl Node for TemplateTransformNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let context =
                minijinja::Value::from_object(TemplateVars { variables: Arc::clone(&self.variables), pool: ctx.pool });
            let output = self
                .env
                .get_template(TEMPLATE_NAME)
                .and_then(|template| template.render(context))
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

    #[tokio::test]
    async fn variables_resolve_like_a_declared_map() {
        let mut pool = VariablePool::new();
        pool.insert("n", "a", Value::from("first"));
        pool.insert("n", "b", Value::from("second"));
        pool.insert("n", "obj", Value::from_json(json!({"x": {"y": 7}})));
        let r = render(
            json!({
                // A repeated name binds its last declaration; a declared variable
                // whose selector is missing is defined (none), an undeclared one is not.
                "template": "{{ v }} {{ obj.x.y }} {{ gone is defined }} {{ gone is none }} {{ other is defined }}",
                "variables": [
                    {"variable": "v", "value_selector": ["n", "a"]},
                    {"variable": "obj", "value_selector": ["n", "obj"]},
                    {"variable": "gone", "value_selector": ["n", "missing"]},
                    {"variable": "v", "value_selector": ["n", "b"]}
                ]
            }),
            pool,
        )
        .await
        .unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("output"), Some(&Value::from("second 7 True True False")));
    }

    #[test]
    fn context_enumerates_distinct_names_in_declaration_order() {
        let variable =
            |name: &str| TemplateVariable { variable: name.to_owned(), value_selector: Selector::of("n", name) };
        let vars = Arc::new(TemplateVars {
            variables: vec![variable("b"), variable("a"), variable("b")].into(),
            pool: VariablePool::new(),
        });
        let Enumerator::Values(names) = vars.enumerate() else { panic!("expected values") };
        assert_eq!(names, [minijinja::Value::from("b"), minijinja::Value::from("a")]);
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
