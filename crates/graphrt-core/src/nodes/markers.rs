//! Scope marker nodes: `iteration-start`, `loop-start` (entry of a container
//! body) and `loop-end` (breaks the enclosing loop when reached; the engine
//! recognizes the `loop-end` kind).

use futures::future::BoxFuture;

use crate::node::{Node, NodeContext, NodeOutput, NodeResult, NodeRole};

/// Entry node of a container body. Does nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScopeStartNode;

impl Node for ScopeStartNode {
    fn role(&self) -> NodeRole {
        NodeRole::ScopeStart
    }

    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().into()) })
    }
}

/// Marks that the enclosing loop should stop after the current round.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoopEndNode;

impl Node for LoopEndNode {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().into()) })
    }
}
