//! `system-one` node: asks TypeSafe's System One API typed questions about a state.
//!
//! ```yaml
//! - id: triage
//!   type: system-one
//!   model: jev-latest
//!   state: "{{#start.message#}}"
//!   questions:
//!     topic:   { type: choice, instructions: "What is the message about?",
//!                criteria: { billing: "Payments, refunds, invoices", bug: "Something is broken" } }
//!     urgency: { type: score, instructions: "How urgent is it?",
//!                criteria: ["Can wait", "This week", "Today"] }
//!     spam:    { type: noul, instructions: "Is this message spam?" }
//!   route_by: topic          # optional: branch on the `topic` choice
//!   min_confidence: 0.6      # optional: below it, take the `low_confidence` handle
//! ```
//!
//! Every string in `state`, `instructions` and `criteria` is a template; a string
//! that is exactly one `{{#..#}}` placeholder is replaced by the typed value.
//! `images` maps names to file variables; each image is attached as an asset and
//! referenced from `state` under its name, so questions can say "`photo`".
//!
//! Outputs: one object per question (the typed answer), `answers` (all of them),
//! `usage` and `model`. With `route_by`, the node is a branch node whose handles
//! are the routed question's options, plus `low_confidence` when a threshold is set.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::file::{DataUrlResolver, FileResolver};
use graphrt_core::node::{ConfigError, NodeResult, NodeRole, parse_config};
use graphrt_core::template::{Part, Template};
use graphrt_core::value::{File, Value};
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector, VariablePool};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value as Json;

use super::common::usage_value;
use crate::typesafe::{
    Answer, DEFAULT_MODEL, ImageAsset, NoulCriteria, Question, SystemOneClient, SystemOneRequest, normalize_format,
};

/// Handle taken when a routed decision falls below `min_confidence` / `min_probability`.
pub const LOW_CONFIDENCE: &str = "low_confidence";

/// Largest image read from a file variable.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// `model` as a plain name or a `{provider, name}` object.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ModelSpec {
    Name(String),
    Object(ModelObject),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelObject {
    #[serde(default)]
    provider: Option<String>,
    name: String,
}

/// One configured question.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionConfig {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub instructions: Option<Json>,
    #[serde(default)]
    pub criteria: Option<Json>,
}

/// Configuration of the `system-one` node.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemOneNodeConfig {
    /// Allow placeholders that refer to missing variables (they become `null`).
    /// Off by default: asking questions about missing input yields confident
    /// answers about nothing.
    #[serde(default)]
    pub allow_missing: bool,
    #[serde(default)]
    model: Option<ModelSpec>,
    /// The content questions refer to (string, object or array; strings are templates).
    #[serde(default)]
    pub state: Option<Json>,
    /// Alternatively, take the state from a variable.
    #[serde(default)]
    pub state_selector: Option<Selector>,
    /// Image variables (file or array of files), keyed by the name used in `state`.
    #[serde(default)]
    pub images: IndexMap<String, Selector>,
    pub questions: IndexMap<String, QuestionConfig>,
    /// Name of a choice question to branch on.
    #[serde(default)]
    pub route_by: Option<String>,
    #[serde(default)]
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub min_probability: Option<f64>,
    /// Send the long-context header (262k-token limits; slower).
    #[serde(default)]
    pub long_context: bool,
}

/// The `system-one` node.
#[derive(Debug)]
pub struct SystemOneNode {
    config: SystemOneNodeConfig,
    model: String,
    /// Routed question's option names (branch handles), when `route_by` is set.
    handles: Vec<String>,
}

impl SystemOneNode {
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: SystemOneNodeConfig = parse_config(doc)?;
        let err = |m: String| Err(ConfigError::new(doc, m));
        if config.questions.is_empty() {
            return err("questions must not be empty".into());
        }
        if config.state.is_some() && config.state_selector.is_some() {
            return err("set either `state` or `state_selector`, not both".into());
        }
        for (name, q) in &config.questions {
            if name.is_empty() {
                return err("question names must not be empty".into());
            }
            if let Err(message) = build_question(q, None) {
                return err(format!("question `{name}`: {message}"));
            }
        }
        for (name, value) in [("min_confidence", config.min_confidence), ("min_probability", config.min_probability)] {
            if let Some(v) = value {
                if !(0.0..=1.0).contains(&v) {
                    return err(format!("{name} must be between 0 and 1"));
                }
            }
        }
        let mut handles = Vec::new();
        match &config.route_by {
            Some(route) => {
                let Some(q) = config.questions.get(route) else {
                    return err(format!("route_by `{route}` is not a configured question"));
                };
                let Ok(Question::Choice { criteria, .. }) = build_question(q, None) else {
                    return err(format!("route_by `{route}` must name a choice question"));
                };
                handles = criteria.keys().cloned().collect();
                if handles.iter().any(|h| h == LOW_CONFIDENCE) {
                    return err(format!("option name `{LOW_CONFIDENCE}` is reserved for the low-confidence handle"));
                }
            }
            None => {
                if config.min_confidence.is_some() || config.min_probability.is_some() {
                    return err("min_confidence and min_probability need route_by".into());
                }
            }
        }
        let model = match &config.model {
            None => DEFAULT_MODEL.to_owned(),
            Some(ModelSpec::Name(n)) if n.trim().is_empty() => return err("`model` must not be empty".into()),
            Some(ModelSpec::Name(n)) => n.trim().to_owned(),
            Some(ModelSpec::Object(o)) => {
                if let Some(p) = o.provider.as_deref().filter(|p| *p != "typesafe") {
                    return err(format!("`model.provider` must be `typesafe` for system-one nodes, got `{p}`"));
                }
                if o.name.trim().is_empty() {
                    return err("`model.name` must not be empty".into());
                }
                o.name.trim().to_owned()
            }
        };
        Ok(Self { config, model, handles })
    }

    fn has_thresholds(&self) -> bool {
        self.config.min_confidence.is_some() || self.config.min_probability.is_some()
    }

    /// Build the request: render templates, resolve images.
    async fn request(&self, ctx: &NodeContext) -> Result<SystemOneRequest, NodeError> {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let mut state = match (&cfg.state_selector, &cfg.state) {
            (Some(sel), _) => pool
                .get(sel)
                .map(|v| v.to_json())
                .ok_or_else(|| NodeError::invalid_input(format!("state variable `{sel}` is missing")))?,
            (None, Some(state)) => render_json(state, pool, cfg.allow_missing)
                .map_err(|m| NodeError::invalid_input(format!("state: {m}")))?,
            (None, None) => Json::Object(serde_json::Map::new()),
        };

        let mut request = SystemOneRequest::new(self.model.clone(), Json::Null);
        request.long_context = cfg.long_context;
        if !cfg.images.is_empty() {
            let mut object = match state {
                Json::Object(map) => map,
                Json::Null => serde_json::Map::new(),
                other => {
                    let mut map = serde_json::Map::new();
                    map.insert("text".to_owned(), other);
                    map
                }
            };
            for (name, selector) in &cfg.images {
                let files = files_at(pool, selector)?;
                let mut refs = Vec::with_capacity(files.len());
                for (i, file) in files.iter().enumerate() {
                    let reference = format!("graphrt-image-{}-{name}-{i}", ctx.exec_id);
                    request.assets.insert(reference.clone(), image_asset(ctx, file).await?);
                    refs.push(Json::String(reference));
                }
                let value = if refs.len() == 1 { refs.remove(0) } else { Json::Array(refs) };
                object.insert(name.clone(), value);
            }
            state = Json::Object(object);
        }
        request.state = state;
        for (name, q) in &cfg.questions {
            let question = build_question(q, Some((pool, cfg.allow_missing)))
                .map_err(|m| NodeError::invalid_input(format!("question `{name}`: {m}")))?;
            request.questions.insert(name.clone(), question);
        }
        Ok(request)
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let client =
            ctx.require_service::<Arc<SystemOneClient>>("TypeSafe client (graphrt_llm::typesafe::SystemOneClient)")?;
        let request = self.request(&ctx).await?;
        let response = client.ask(&request).await?;

        let usage = response.usage.to_usage();
        let mut output = NodeOutput::new();
        let mut all = graphrt_core::value::Map::new();
        for (name, answer) in &response.answers {
            if !self.config.questions.contains_key(name) {
                continue;
            }
            let value = serde_json::to_value(answer).map(Value::from_json).unwrap_or(Value::Null);
            all.insert(name.clone(), value.clone());
            output = output.with(name.clone(), value);
        }
        output = output
            .with("answers", Value::from(all))
            .with("usage", usage_value(&usage))
            .with("model", response.model.as_str())
            .usage(usage);

        if let Some(route) = &self.config.route_by {
            let Some(Answer::Choice { choice, confidence, probabilities }) = response.answer(route) else {
                return Err(NodeError::new("model_decode", format!("no choice answer for routed question `{route}`")));
            };
            if !self.handles.iter().any(|h| h == choice) {
                return Err(NodeError::new("model_decode", format!("System One chose unknown option `{choice}`")));
            }
            let probability = probabilities.get(choice).copied().ok_or_else(|| {
                NodeError::new("model_decode", format!("no probability for the chosen option `{choice}`"))
            })?;
            let low = self.config.min_confidence.is_some_and(|m| *confidence < m)
                || self.config.min_probability.is_some_and(|m| probability < m);
            if low {
                output = output.branch(LOW_CONFIDENCE).meta("predicted", choice.as_str());
            } else {
                output = output.branch(choice.clone());
            }
        }
        Ok(output.into())
    }
}

/// Convert a question config to a typed question. With a pool, templates are rendered;
/// without one, only the shape is validated.
fn build_question(q: &QuestionConfig, pool: Option<(&VariablePool, bool)>) -> Result<Question, String> {
    let render = |v: &Json| -> Result<Json, String> {
        match pool {
            Some((p, allow_missing)) => render_json(v, p, allow_missing),
            None => Ok(v.clone()),
        }
    };
    let instructions = q.instructions.as_ref().map(render).transpose()?;
    match q.kind.as_str() {
        "choice" => {
            let criteria = match &q.criteria {
                Some(Json::Object(map)) => {
                    let mut out = IndexMap::new();
                    for (k, v) in map {
                        out.insert(k.clone(), render(v)?);
                    }
                    out
                }
                // A list of option names: each option is interpreted by its name alone.
                Some(Json::Array(items)) => {
                    let mut map = IndexMap::new();
                    for item in items {
                        let Some(name) = item.as_str().filter(|s| !s.is_empty()) else {
                            return Err("choice options listed as an array must be non-empty strings".into());
                        };
                        map.insert(name.to_owned(), Json::Null);
                    }
                    map
                }
                _ => return Err("choice questions need `criteria` (option -> description)".into()),
            };
            if criteria.is_empty() || criteria.len() > crate::typesafe::MAX_CHOICE_OPTIONS {
                return Err(format!("choice questions need 1 to {} options", crate::typesafe::MAX_CHOICE_OPTIONS));
            }
            Ok(Question::Choice { instructions, criteria })
        }
        "score" => match &q.criteria {
            Some(Json::Array(levels)) if !levels.is_empty() && levels.len() <= crate::typesafe::MAX_SCORE_LEVELS => {
                let criteria = levels.iter().map(render).collect::<Result<Vec<_>, _>>()?;
                Ok(Question::Score { instructions, criteria })
            }
            _ => Err(format!(
                "score questions need `criteria`: 1 to {} ordered levels",
                crate::typesafe::MAX_SCORE_LEVELS
            )),
        },
        "noul" => {
            let criteria = match &q.criteria {
                None | Some(Json::Null) => None,
                Some(Json::Object(map)) => {
                    if let Some(key) = map.keys().find(|k| *k != "true" && *k != "false") {
                        return Err(format!("noul `criteria` keys must be `true` / `false`, got `{key}`"));
                    }
                    Some(NoulCriteria {
                        yes: map.get("true").map(render).transpose()?,
                        no: map.get("false").map(render).transpose()?,
                    })
                }
                Some(_) => return Err("noul `criteria` must be an object with `true` / `false`".into()),
            };
            Ok(Question::Noul { instructions, criteria })
        }
        "box" => {
            if q.criteria.is_some() {
                return Err("box questions take no criteria".into());
            }
            Ok(Question::Box { instructions })
        }
        other => Err(format!("unknown question type `{other}` (choice, score, noul or box)")),
    }
}

/// Render every string in a JSON value as a template. A string that is exactly one
/// placeholder becomes the typed value; secrets stay masked.
///
/// A placeholder whose variable is missing is an error unless `allow_missing`.
fn render_json(value: &Json, pool: &VariablePool, allow_missing: bool) -> Result<Json, String> {
    Ok(match value {
        Json::String(s) => {
            let template = Template::parse(s);
            if !allow_missing {
                if let Some(missing) = template.selectors().find(|sel| pool.get(sel).is_none()) {
                    return Err(format!(
                        "variable `{missing}` is missing (set `allow_missing: true` to send it as null)"
                    ));
                }
            }
            match template.parts() {
                [Part::Var(sel)] => pool.get(sel).map(|v| v.to_json()).unwrap_or(Json::Null),
                _ if template.is_static() => value.clone(),
                _ => Json::String(template.render(pool)),
            }
        }
        Json::Array(items) => {
            Json::Array(items.iter().map(|v| render_json(v, pool, allow_missing)).collect::<Result<_, _>>()?)
        }
        Json::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), render_json(v, pool, allow_missing)?);
            }
            Json::Object(out)
        }
        other => other.clone(),
    })
}

fn json_selectors(value: &Json, out: &mut Vec<Selector>) {
    match value {
        Json::String(s) => out.extend(Template::parse(s).selectors().cloned()),
        Json::Array(items) => items.iter().for_each(|v| json_selectors(v, out)),
        Json::Object(map) => map.values().for_each(|v| json_selectors(v, out)),
        _ => {}
    }
}

/// Files referenced by an image variable.
fn files_at(pool: &VariablePool, selector: &Selector) -> Result<Vec<File>, NodeError> {
    match pool.get(selector) {
        Some(Value::File(f)) => Ok(vec![(*f).clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::File(f) => Ok((**f).clone()),
                other => Err(NodeError::invalid_input(format!(
                    "image variable `{selector}` contains a {} instead of a file",
                    other.type_name()
                ))),
            })
            .collect(),
        Some(other) => Err(NodeError::invalid_input(format!(
            "image variable `{selector}` is a {}, expected a file",
            other.type_name()
        ))),
        None => Err(NodeError::invalid_input(format!("image variable `{selector}` is missing"))),
    }
}

/// Read an image file and turn it into an asset (png or jpg).
async fn image_asset(ctx: &NodeContext, file: &File) -> Result<ImageAsset, NodeError> {
    let resolver: Option<&Arc<dyn FileResolver>> = ctx.service::<Arc<dyn FileResolver>>();
    let read = match resolver {
        Some(r) => r.read(file, MAX_IMAGE_BYTES).await,
        None => DataUrlResolver.read(file, MAX_IMAGE_BYTES).await,
    };
    let bytes = read.map_err(|e| {
        let error = NodeError::new("file", e.to_string());
        if e.is_retryable() { error } else { NodeError::fatal("file", e.to_string()) }
    })?;
    let format = bytes
        .mime_type
        .as_deref()
        .and_then(normalize_format)
        .or_else(|| file.mime_type.as_deref().and_then(normalize_format))
        .or_else(|| file.extension.as_deref().and_then(normalize_format))
        .or_else(|| sniff_format(&bytes.bytes))
        .ok_or_else(|| NodeError::invalid_input(format!("image `{}` must be PNG or JPEG", file.name)))?;
    ImageAsset::new(format, &bytes.bytes).map_err(NodeError::from)
}

fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

impl Node for SystemOneNode {
    fn role(&self) -> NodeRole {
        if self.config.route_by.is_some() { NodeRole::Branch } else { NodeRole::Executable }
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let cfg = &self.config;
        let mut out = Vec::new();
        if let Some(state) = &cfg.state {
            json_selectors(state, &mut out);
        }
        out.extend(cfg.state_selector.iter().cloned());
        out.extend(cfg.images.values().cloned());
        for q in cfg.questions.values() {
            if let Some(i) = &q.instructions {
                json_selectors(i, &mut out);
            }
            if let Some(c) = &q.criteria {
                json_selectors(c, &mut out);
            }
        }
        out
    }
}

impl SystemOneNode {
    /// Branch handles (routed options, plus `low_confidence` when thresholds are set).
    pub fn handles(&self) -> Vec<String> {
        let mut handles = self.handles.clone();
        if self.has_thresholds() {
            handles.push(LOW_CONFIDENCE.to_owned());
        }
        handles
    }
}

/// Factory for the node registry.
pub fn factory(doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
    Ok(Arc::new(SystemOneNode::from_doc(doc)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(config: Json) -> NodeDoc {
        NodeDoc::new("s1", "system-one", config)
    }

    #[test]
    fn config_validation() {
        let ok = SystemOneNode::from_doc(&doc(serde_json::json!({
            "state": "{{#start.text#}}",
            "questions": {
                "topic": {"type": "choice", "criteria": {"billing": "Money", "bug": null}},
                "urgency": {"type": "score", "criteria": ["low", "high"]},
                "spam": {"type": "noul", "instructions": "Is it spam?", "criteria": {"true": "ads", "false": "real"}}
            },
            "route_by": "topic",
            "min_confidence": 0.5
        })))
        .unwrap();
        assert_eq!(ok.role(), NodeRole::Branch);
        assert_eq!(ok.handles(), vec!["billing", "bug", LOW_CONFIDENCE]);
        assert_eq!(ok.model, DEFAULT_MODEL);
        assert!(ok.inputs().iter().any(|s| s.to_string() == "start.text"));

        for bad in [
            serde_json::json!({"questions": {}}),
            serde_json::json!({"questions": {"q": {"type": "choice"}}}),
            serde_json::json!({"questions": {"q": {"type": "score", "criteria": []}}}),
            serde_json::json!({"questions": {"q": {"type": "nope"}}}),
            serde_json::json!({"questions": {"q": {"type": "noul"}}, "route_by": "q"}),
            serde_json::json!({"questions": {"q": {"type": "noul"}}, "route_by": "missing"}),
            serde_json::json!({"questions": {"q": {"type": "noul"}}, "min_confidence": 0.5}),
            serde_json::json!({"questions": {"q": {"type": "choice", "criteria": ["a"]}}, "route_by": "q", "min_confidence": 2}),
        ] {
            assert!(SystemOneNode::from_doc(&doc(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn renders_templates_typed() {
        let mut pool = VariablePool::new();
        pool.insert("n", "obj", Value::from_json(serde_json::json!({"a": 1})));
        pool.insert("n", "name", Value::from("Ada"));
        let rendered = render_json(
            &serde_json::json!({"whole": "{{#n.obj#}}", "text": "hi {{#n.name#}}", "plain": "x", "num": 3}),
            &pool,
            false,
        )
        .unwrap();
        assert_eq!(rendered, serde_json::json!({"whole": {"a": 1}, "text": "hi Ada", "plain": "x", "num": 3}));
        // Missing variables are errors unless explicitly allowed.
        let missing = serde_json::json!({"text": "{{#n.nope#}}"});
        assert!(render_json(&missing, &pool, false).is_err());
        assert_eq!(render_json(&missing, &pool, true).unwrap(), serde_json::json!({"text": null}));
    }

    #[test]
    fn typos_and_bad_criteria_are_rejected() {
        for bad in [
            serde_json::json!({"questions": {"q": {"type": "noul"}}, "min_confidance": 0.5}),
            serde_json::json!({"questions": {"q": {"type": "noul", "instruction": "x"}}}),
            serde_json::json!({"questions": {"q": {"type": "noul", "criteria": {"yes": "a", "no": "b"}}}}),
            serde_json::json!({"model": {"provider": "openai", "name": "jev"}, "questions": {"q": {"type": "noul"}}}),
            serde_json::json!({"model": {"name": " "}, "questions": {"q": {"type": "noul"}}}),
            serde_json::json!({"model": "", "questions": {"q": {"type": "noul"}}}),
        ] {
            assert!(SystemOneNode::from_doc(&doc(bad.clone())).is_err(), "{bad}");
        }
    }
}
