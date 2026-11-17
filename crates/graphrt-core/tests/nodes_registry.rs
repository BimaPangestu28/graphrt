//! Every built-in node kind builds from a workflow document, and a graph using
//! all of them (including container scopes) validates.

use graphrt_core::node::NodeRole;
use graphrt_core::{Graph, Registry, Workflow};

const WORKFLOW: &str = r#"
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: items, type: json, required: true }] }
    - id: check
      type: if-else
      cases:
        - case_id: has
          logical_operator: and
          conditions:
            - { variable_selector: [start, items], comparison_operator: not empty }
    - id: it
      type: iteration
      iterator_selector: [start, items]
      output_selector: [tt, output]
      is_parallel: true
      parallel_nums: 4
    - { id: it_start, type: iteration-start, parent: it }
    - id: tt
      type: template-transform
      parent: it
      template: "{{ x }}!"
      variables: [{ variable: x, value_selector: [it, item] }]
    - id: lp
      type: loop
      loop_count: 3
      loop_variables: [{ label: count, var_type: number, value_type: constant, value: 0 }]
    - { id: lp_start, type: loop-start, parent: lp }
    - id: inc
      type: assigner
      parent: lp
      items: [{ variable_selector: [lp, count], operation: "+=", value: 1 }]
    - { id: lp_end, type: loop-end, parent: lp }
    - id: lo
      type: list-operator
      variable: [it, output]
      limit: { enabled: true, size: 1 }
    - { id: agg, type: variable-aggregator, variables: [[lo, first_record], [start, items]] }
    - id: human
      type: human-input
      actions: [{ id: ok }]
    - { id: ans, type: answer, answer: "{{#agg.output#}}" }
    - { id: end, type: end, outputs: [{ variable: out, value_selector: [agg, output] }] }
  edges:
    - { source: start, target: check }
    - { source: check, target: it, sourceHandle: has }
    - { source: check, target: agg, sourceHandle: "false" }
    - { source: it_start, target: tt }
    - { source: it, target: lp }
    - { source: lp_start, target: inc }
    - { source: inc, target: lp_end }
    - { source: lp, target: lo }
    - { source: lo, target: agg }
    - { source: agg, target: human }
    - { source: human, target: ans, sourceHandle: ok }
    - { source: ans, target: end }
"#;

#[test]
fn builtin_graph_builds() {
    let wf = Workflow::parse(WORKFLOW).unwrap();
    let graph = Graph::build(wf, &Registry::with_builtins()).unwrap();
    assert!(graph.warnings().is_empty(), "{:?}", graph.warnings());
    let role = |id: &str| graph.node_by_id(id).map(|n| n.role);
    assert_eq!(role("start"), Some(NodeRole::Root));
    assert_eq!(role("check"), Some(NodeRole::Branch));
    assert_eq!(role("it"), Some(NodeRole::Container));
    assert_eq!(role("lp"), Some(NodeRole::Container));
    assert_eq!(role("it_start"), Some(NodeRole::ScopeStart));
    assert_eq!(role("human"), Some(NodeRole::Branch));
    assert_eq!(role("ans"), Some(NodeRole::Response));
    let it = graph.index_of("it").unwrap();
    let body = graph.scope_of(it).unwrap();
    assert_eq!(graph.node(body.entry).map(|n| n.id.as_ref()), Some("it_start"));
}

#[test]
fn all_kinds_registered() {
    let r = Registry::with_builtins();
    for kind in [
        "start",
        "end",
        "answer",
        "if-else",
        "template-transform",
        "variable-aggregator",
        "assigner",
        "variable-assigner",
        "list-operator",
        "iteration",
        "iteration-start",
        "loop",
        "loop-start",
        "loop-end",
        "human-input",
    ] {
        assert!(r.contains(kind), "missing {kind}");
    }
}

#[test]
fn invalid_config_is_reported_with_node_id() {
    let wf = Workflow::parse(
        "graph:\n  nodes:\n    - { id: s, type: start }\n    - { id: lp, type: loop, loop_count: 0 }\n  edges:\n    - { source: s, target: lp }\n",
    )
    .unwrap();
    let err = Graph::build(wf, &Registry::with_builtins()).unwrap_err().to_string();
    assert!(err.contains("lp"), "{err}");
}

#[test]
fn variable_assigner_kind_detects_shape() {
    let wf = Workflow::parse(
        r#"
graph:
  nodes:
    - { id: s, type: start }
    - { id: agg, type: variable-assigner, variables: [[s, a]] }
    - { id: asg, type: variable-assigner, items: [{ variable_selector: [conversation, x], operation: clear }] }
  edges:
    - { source: s, target: agg }
    - { source: agg, target: asg }
"#,
    )
    .unwrap();
    assert!(Graph::build(wf, &Registry::with_builtins()).is_ok());
}
