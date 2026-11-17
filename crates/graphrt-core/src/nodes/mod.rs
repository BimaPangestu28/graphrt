//! Built-in nodes.
//!
//! | kind | role |
//! |---|---|
//! | `start` | root |
//! | `end` | response |
//! | `answer` | response |
//! | `if-else` | branch |
//! | `template-transform` | executable (feature `jinja`) |
//! | `variable-aggregator` | executable |
//! | `assigner` | executable |
//! | `list-operator` | executable |
//! | `iteration` | container |
//! | `loop` | container |
//! | `iteration-start`, `loop-start`, `loop-end` | scope markers |
//! | `human-input` | branch |
//!
//! Node implementations are internal; they are built from workflow documents
//! through [`register_builtins`] / [`crate::node::Registry::with_builtins`].
//! Configuration fields are documented in the workflow format reference.
//!
//! `variable-assigner` is accepted for both assigner configs (`items`,
//! `assigned_variable_selector`) and aggregator configs (`variables`),
//! detected by the configuration shape.

use std::sync::Arc;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, Registry, parse_config};

mod aggregator;
mod answer;
mod assigner;
mod end;
mod human_input;
mod if_else;
mod iteration;
mod list_operator;
mod loop_node;
mod markers;
mod start;
#[cfg(feature = "jinja")]
mod template_transform;

/// Register every built-in core node kind.
pub fn register_builtins(registry: &mut Registry) {
    registry.register("start", |doc| Ok(Arc::new(start::StartNode::new(doc, parse_config(doc)?)?)));
    registry.register("end", |doc| Ok(Arc::new(end::EndNode::new(parse_config(doc)?))));
    registry.register("answer", |doc| Ok(Arc::new(answer::AnswerNode::new(parse_config(doc)?))));
    registry.register("if-else", |doc| Ok(Arc::new(if_else::IfElseNode::new(doc, parse_config(doc)?)?)));
    registry.register("variable-aggregator", |doc| Ok(Arc::new(aggregator::AggregatorNode::new(parse_config(doc)?))));
    registry.register("assigner", build_assigner);
    registry.register("variable-assigner", |doc| {
        let is_assigner = doc.config.contains_key("items") || doc.config.contains_key("assigned_variable_selector");
        if is_assigner {
            build_assigner(doc)
        } else {
            Ok(Arc::new(aggregator::AggregatorNode::new(parse_config(doc)?)))
        }
    });
    registry
        .register("list-operator", |doc| Ok(Arc::new(list_operator::ListOperatorNode::new(doc, parse_config(doc)?)?)));
    registry.register("iteration", |doc| Ok(Arc::new(iteration::IterationNode::new(doc, parse_config(doc)?)?)));
    registry.register("loop", |doc| Ok(Arc::new(loop_node::LoopNode::new(doc, parse_config(doc)?)?)));
    registry.register("iteration-start", |_| Ok(Arc::new(markers::ScopeStartNode)));
    registry.register("loop-start", |_| Ok(Arc::new(markers::ScopeStartNode)));
    registry.register("loop-end", |_| Ok(Arc::new(markers::LoopEndNode)));
    registry.register("human-input", |doc| Ok(Arc::new(human_input::HumanInputNode::new(doc, parse_config(doc)?)?)));
    crate::tool_node::register(registry);
    #[cfg(feature = "jinja")]
    registry.register("template-transform", |doc| {
        Ok(Arc::new(template_transform::TemplateTransformNode::new(doc, parse_config(doc)?)?))
    });
}

fn build_assigner(doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
    Ok(Arc::new(assigner::AssignerNode::new(doc, parse_config(doc)?)?))
}
