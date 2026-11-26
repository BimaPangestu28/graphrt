//! `human-input`: pauses the run until a person submits a form or picks an action.
//!
//! First run: returns a pause request of kind `human_input` whose data holds
//! the rendered prompt, the form fields, the available actions and the
//! optional timeout. The host persists the run snapshot and shows the form.
//!
//! Resume: the host passes `{ "action": "<id>", "values": { ... } }` as the
//! node's resume input. The node validates the action and required fields,
//! outputs each field value plus `action`, and selects the branch whose handle
//! equals the action id. Resuming without input pauses again.
//!
//! `card` (optional) adds a UI block (see [`crate::ui`]) to the pause data, e.g. an
//! Adaptive Card the host renders in place of the plain form: either the block itself or a
//! `{{#node.output#}}` reference to one. The host answers with the card action's id as
//! `action` and the card's inputs as `values`, so a card's submit actions must use the
//! node's action ids. Leave `form` empty with a card: every value is then an output.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{
    ConfigError, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, PauseRequest,
};
use crate::pool::Selector;
use crate::template::Template;
use crate::value::Value;

/// Action used when none are configured.
pub const DEFAULT_ACTION: &str = "submit";

/// `human-input` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HumanInputConfig {
    /// Message shown to the person; may contain `{{#..#}}` placeholders.
    #[serde(default, alias = "content")]
    pub prompt: Option<Template>,
    #[serde(default)]
    pub form: Option<Form>,
    /// Alternative to `form.fields`.
    #[serde(default)]
    pub inputs: Vec<Field>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// How long the host should wait before expiring the request.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// A UI block shown with the question, or a `{{#..#}}` reference to one.
    #[serde(default)]
    pub card: Option<serde_json::Value>,
}

/// Form definition.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Form {
    #[serde(default)]
    pub fields: Vec<Field>,
}

/// A form field.
#[derive(Clone, Debug, Deserialize)]
pub struct Field {
    #[serde(alias = "variable")]
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "type", default = "text_kind")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    /// Default value; strings may contain `{{#..#}}` placeholders.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

fn text_kind() -> String {
    "text".to_owned()
}

/// A button / decision the person can choose.
#[derive(Clone, Debug, Deserialize)]
pub struct Action {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// The `human-input` node.
#[derive(Debug)]
pub struct HumanInputNode {
    prompt: Option<Template>,
    fields: Vec<Field>,
    actions: Vec<Action>,
    timeout_ms: Option<u64>,
    card: Option<serde_json::Value>,
}

impl HumanInputNode {
    pub fn new(doc: &NodeDoc, config: HumanInputConfig) -> Result<Self, ConfigError> {
        let mut fields = config.form.map(|f| f.fields).unwrap_or_default();
        fields.extend(config.inputs);
        let actions = if config.actions.is_empty() {
            vec![Action { id: DEFAULT_ACTION.to_owned(), title: Some("Submit".to_owned()) }]
        } else {
            config.actions
        };
        let mut seen = std::collections::HashSet::new();
        for a in &actions {
            if a.id.is_empty() || !seen.insert(a.id.as_str()) {
                return Err(ConfigError::new(doc, format!("action ids must be unique and non-empty (`{}`)", a.id)));
            }
        }
        Ok(Self { prompt: config.prompt, fields, actions, timeout_ms: config.timeout_ms, card: config.card })
    }

    fn pause(&self, ctx: &NodeContext) -> PauseRequest {
        let fields = self.fields.iter().map(|f| {
            let default = match &f.default {
                Some(serde_json::Value::String(s)) => Template::parse(s).resolve(&ctx.pool),
                Some(other) => Value::from_json(other.clone()),
                None => Value::Null,
            };
            Value::object([
                ("name", Value::from(f.name.as_str())),
                ("label", Value::from(f.label.clone().unwrap_or_else(|| f.name.clone()))),
                ("type", Value::from(f.kind.as_str())),
                ("required", Value::Bool(f.required)),
                ("default", default),
            ])
        });
        let actions = self.actions.iter().map(|a| {
            Value::object([
                ("id", Value::from(a.id.as_str())),
                ("title", Value::from(a.title.clone().unwrap_or_else(|| a.id.clone()))),
            ])
        });
        let mut data = Value::object([
            ("prompt", self.prompt.as_ref().map(|t| Value::from(t.render(&ctx.pool))).unwrap_or(Value::Null)),
            ("fields", Value::array(fields)),
            ("actions", Value::array(actions)),
            ("timeout_ms", self.timeout_ms.and_then(|t| i64::try_from(t).ok()).map(Value::Int).unwrap_or(Value::Null)),
        ]);
        if let (Some(card), Value::Object(map)) = (&self.card, &mut data) {
            std::sync::Arc::make_mut(map).insert("card".to_owned(), self.resolve_card(card, ctx));
        }
        PauseRequest { kind: "human_input".to_owned(), data, state: serde_json::Value::Null }
    }

    /// The configured card, when it is (or refers to) a UI block; `null` otherwise.
    fn resolve_card(&self, card: &serde_json::Value, ctx: &NodeContext) -> Value {
        let resolved = match card {
            serde_json::Value::String(s) => Template::parse(s).resolve(&ctx.pool),
            other => Value::from_json(other.clone()),
        };
        if crate::ui::ui_block_kind(&resolved).is_some() { resolved } else { Value::Null }
    }

    fn complete(&self, input: &Value) -> Result<NodeOutput, NodeError> {
        let action = match input.get_path(&["action"]) {
            Some(Value::String(s)) => s.to_string(),
            None | Some(Value::Null) if self.actions.len() == 1 => {
                self.actions.first().map(|a| a.id.clone()).unwrap_or_default()
            }
            _ => return Err(NodeError::invalid_input("human input must include an `action`")),
        };
        if !self.actions.iter().any(|a| a.id == action) {
            return Err(NodeError::invalid_input(format!("unknown action `{action}`")));
        }
        let values = input.get_path(&["values"]).unwrap_or(Value::Null);
        let mut out = NodeOutput::new();
        if self.fields.is_empty() {
            if let Some(map) = values.as_object() {
                for (k, v) in map.iter() {
                    out.outputs.insert(k.clone(), v.clone());
                }
            }
        } else {
            for f in &self.fields {
                let v = values.get_path(&[f.name.as_str()]).unwrap_or(Value::Null);
                if f.required && v.is_empty() {
                    return Err(NodeError::invalid_input(format!("field `{}` is required", f.name)));
                }
                out.outputs.insert(f.name.clone(), v);
            }
        }
        out.outputs.insert("action".into(), Value::from(action.as_str()));
        Ok(out.branch(action))
    }
}

impl Node for HumanInputNode {
    fn role(&self) -> NodeRole {
        NodeRole::Branch
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            match ctx.resume.as_ref().and_then(|r| r.input.as_ref()) {
                Some(input) if !input.is_null() => self.complete(input).map(NodeOutcome::Completed),
                _ => Ok(NodeOutcome::Paused(self.pause(&ctx))),
            }
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out: Vec<Selector> = self.prompt.iter().flat_map(|t| t.selectors().cloned()).collect();
        for field in &self.fields {
            if let Some(default) = &field.default {
                crate::condition::expected_selectors(default, &mut out);
            }
        }
        if let Some(card) = &self.card {
            crate::condition::expected_selectors(card, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Resume;
    use crate::pool::VariablePool;
    use serde_json::json;

    fn node(cfg: serde_json::Value) -> HumanInputNode {
        let doc = NodeDoc::new("h", "human-input", cfg);
        HumanInputNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    fn ctx(input: Option<serde_json::Value>) -> NodeContext {
        let mut pool = VariablePool::new();
        pool.insert("llm", "text", Value::from("draft"));
        NodeContext::for_test("h", pool)
            .with_resume(input.map(|i| Resume { state: serde_json::Value::Null, input: Some(Value::from_json(i)) }))
    }

    fn cfg() -> serde_json::Value {
        json!({
            "prompt": "Review: {{#llm.text#}}",
            "form": {"fields": [{"name": "comment", "required": true, "default": "{{#llm.text#}}"}]},
            "actions": [{"id": "approve", "title": "Approve"}, {"id": "reject"}],
            "timeout_ms": 60000
        })
    }

    #[tokio::test]
    async fn pauses_with_rendered_form() {
        let r = node(cfg()).run(ctx(None)).await.unwrap();
        let NodeOutcome::Paused(p) = r else { panic!("{r:?}") };
        assert_eq!(p.kind, "human_input");
        assert_eq!(p.data.get_path(&["prompt"]), Some(Value::from("Review: draft")));
        assert_eq!(p.data.get_path(&["fields", "0", "default"]), Some(Value::from("draft")));
        assert_eq!(p.data.get_path(&["actions", "length"]), Some(Value::Int(2)));
    }

    #[tokio::test]
    async fn pause_carries_the_referenced_card_and_card_values_pass_through() {
        let mut pool = VariablePool::new();
        pool.insert("cards", "data", Value::from_json(json!({"$ui": "adaptive_card", "version": 1, "card": {}})));
        pool.insert("llm", "text", Value::from("not a block"));
        let n = node(json!({"card": "{{#cards.data#}}", "actions": [{"id": "approve"}]}));
        assert!(n.inputs().iter().any(|s| s.to_string().contains("cards")), "{:?}", n.inputs());
        let r = n.run(NodeContext::for_test("h", pool.clone())).await.unwrap();
        let NodeOutcome::Paused(p) = r else { panic!("{r:?}") };
        assert_eq!(p.data.get_path(&["card", "$ui"]), Some(Value::from("adaptive_card")));

        let not_a_block = node(json!({"card": "{{#llm.text#}}"}));
        let NodeOutcome::Paused(p) = not_a_block.run(NodeContext::for_test("h", pool)).await.unwrap() else { panic!() };
        assert_eq!(p.data.get_path(&["card"]), Some(Value::Null));

        let without_card = node(cfg());
        let NodeOutcome::Paused(p) = without_card.run(ctx(None)).await.unwrap() else { panic!() };
        assert_eq!(p.data.get_path(&["card"]), None, "no `card` key unless configured");

        let answer = json!({"action": "approve", "values": {"amount": 120, "note": "ok"}});
        let resumed = n.run(ctx(Some(answer))).await.unwrap();
        let NodeOutcome::Completed(o) = resumed else { panic!("{resumed:?}") };
        assert_eq!(o.outputs.get("amount"), Some(&Value::Int(120)));
        assert_eq!(o.branch.as_deref(), Some("approve"));
    }

    #[tokio::test]
    async fn completes_with_action_branch() {
        let r = node(cfg()).run(ctx(Some(json!({"action": "reject", "values": {"comment": "no"}})))).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!("{r:?}") };
        assert_eq!(o.branch.as_deref(), Some("reject"));
        assert_eq!(o.outputs.get("comment"), Some(&Value::from("no")));
        assert_eq!(o.outputs.get("action"), Some(&Value::from("reject")));
    }

    #[tokio::test]
    async fn validates_input() {
        let n = node(cfg());
        assert!(n.run(ctx(Some(json!({"action": "nope", "values": {"comment": "x"}})))).await.is_err());
        assert!(n.run(ctx(Some(json!({"action": "approve", "values": {}})))).await.is_err());
        // Single default action may be omitted.
        let n = node(json!({}));
        let r = n.run(ctx(Some(json!({"values": {"a": 1}})))).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.branch.as_deref(), Some(DEFAULT_ACTION));
        assert_eq!(o.outputs.get("a"), Some(&Value::Int(1)));
    }

    #[test]
    fn duplicate_actions_rejected() {
        let doc = NodeDoc::new("h", "human-input", json!({"actions": [{"id": "a"}, {"id": "a"}]}));
        assert!(HumanInputNode::new(&doc, doc.parse_config().unwrap()).is_err());
    }
}
