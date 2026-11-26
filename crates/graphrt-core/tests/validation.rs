//! Whole-document validation: every error collected, with paths, positions and suggestions.
// The documents below are YAML, which needs the `yaml` feature.
#![cfg(feature = "yaml")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use graphrt_core::validate::{Diagnostic, Severity, validate_text};
use graphrt_core::{Graph, Registry, Workflow};

fn check(text: &str) -> Vec<Diagnostic> {
    validate_text(text, &Registry::with_builtins()).diagnostics
}

fn errors(text: &str) -> Vec<Diagnostic> {
    check(text).into_iter().filter(Diagnostic::is_error).collect()
}

const GOOD: &str = r#"version: "1"
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: name, type: string }]
    - id: greet
      type: template-transform
      template: "Hello {{ name }}"
      variables: [{ variable: name, value_selector: [start, name] }]
    - id: end
      type: end
      outputs: [{ variable: greeting, value_selector: [greet, output] }]
  edges:
    - { source: start, target: greet }
    - { source: greet, target: end }
"#;

#[test]
fn valid_document_has_no_findings_and_builds() {
    let v = validate_text(GOOD, &Registry::with_builtins());
    assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
    assert!(v.graph.is_some());
}

/// The validator compiles a clean document from the nodes it already built; the
/// result must be the same graph `Graph::build` produces, fingerprint included.
#[test]
fn validated_graph_matches_a_direct_build() {
    let registry = Registry::with_builtins();
    let validated = validate_text(GOOD, &registry).graph.expect("valid document builds");
    let direct = Graph::build(Workflow::parse(GOOD).unwrap(), &registry).unwrap();
    assert_eq!(validated.fingerprint(), direct.fingerprint());
    assert_eq!(validated.fingerprint().len(), 16);
    let ids = |g: &Graph| g.nodes().iter().map(|n| (n.id.to_string(), n.kind.to_string())).collect::<Vec<_>>();
    assert_eq!(ids(&validated), ids(&direct));
    assert_eq!(validated.edges().len(), direct.edges().len());
    assert_eq!(validated.warnings(), direct.warnings());
}

#[test]
fn misspelled_config_field_is_reported_with_position_and_suggestion() {
    let text = GOOD.replace("      template: \"Hello", "      templat: \"Hello");
    let found = errors(&text);
    assert_eq!(found.len(), 1, "{found:?}");
    let d = &found[0];
    assert_eq!(d.code, "unknown-field");
    assert_eq!(d.path.to_string(), "graph.nodes[1].templat");
    assert_eq!(d.suggestion.as_deref(), Some("template"));
    let span = d.span.expect("position");
    assert_eq!((span.line, span.column, span.len), (9, 7, 7));
    assert_eq!(d.node.as_deref(), Some("greet"));
    // The library entry point rejects it too.
    let wf = Workflow::parse(&text).unwrap();
    let err = Graph::build(wf, &Registry::with_builtins()).unwrap_err().to_string();
    assert!(err.contains("templat") && err.contains("did you mean `template`"), "{err}");
}

#[test]
fn collects_every_error() {
    let text = GOOD
        .replace("      template: \"Hello", "      templat: \"Hello")
        .replace("{ source: greet, target: end }", "{ source: gret, target: end }")
        .replace("value_selector: [start, name]", "value_selector: [strat, name]")
        .replace("type: end", "type: ned");
    let found = errors(&text);
    let codes: Vec<&str> = found.iter().map(|d| d.code).collect();
    assert!(codes.contains(&"unknown-field"), "{codes:?}");
    assert!(codes.contains(&"unknown-node-type"), "{codes:?}");
    assert!(codes.contains(&"unknown-reference"), "{codes:?}");
    assert!(codes.contains(&"unknown-node"), "{codes:?}");
    let edge = found.iter().find(|d| d.code == "unknown-node").unwrap();
    assert_eq!(edge.path.to_string(), "graph.edges[1].source");
    assert_eq!(edge.suggestion.as_deref(), Some("greet"));
    assert_eq!(edge.span.map(|s| s.line), Some(16));
    let kind = found.iter().find(|d| d.code == "unknown-node-type").unwrap();
    assert_eq!(kind.suggestion.as_deref(), Some("end"));
    let reference = found.iter().find(|d| d.code == "unknown-reference").unwrap();
    assert_eq!(reference.path.to_string(), "graph.nodes[1].variables[0].value_selector[0]");
    assert_eq!(reference.suggestion.as_deref(), Some("start"));
    // Sorted by position.
    let lines: Vec<u32> = found.iter().filter_map(|d| d.span.map(|s| s.line)).collect();
    assert!(lines.windows(2).all(|w| w[0] <= w[1]), "{lines:?}");
}

#[test]
fn unknown_handles_are_errors_with_suggestions() {
    let text = r#"
graph:
  nodes:
    - { id: start, type: start }
    - id: check
      type: if-else
      cases: [{ case_id: urgent, conditions: [] }]
    - { id: a, type: answer, answer: x }
    - { id: b, type: answer, answer: y }
  edges:
    - { source: start, target: check }
    - { source: check, target: a, handle: urgnet }
    - { source: check, target: b, handle: "false" }
"#;
    let found = errors(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].code, "unknown-handle");
    assert_eq!(found[0].path.to_string(), "graph.edges[1].handle");
    assert_eq!(found[0].suggestion.as_deref(), Some("urgent"));
}

#[test]
fn non_branch_nodes_only_have_the_source_handle() {
    let text = "graph:\n  nodes:\n    - { id: s, type: start }\n    - { id: a, type: answer, answer: x }\n  edges:\n    - { source: s, target: a, handle: sourc }\n";
    let found = errors(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].code, "unknown-handle");
    assert_eq!(found[0].suggestion.as_deref(), Some("source"));
}

#[test]
fn dead_fail_branch_edges_are_warnings() {
    let text = "graph:\n  nodes:\n    - { id: s, type: start }\n    - { id: a, type: answer, answer: x }\n  edges:\n    - { source: s, target: a, handle: fail-branch }\n";
    let found = check(text);
    assert!(found.iter().all(|d| d.severity == Severity::Warning), "{found:?}");
    assert!(found.iter().any(|d| d.code == "dead-edge"), "{found:?}");
}

#[test]
fn document_level_typos_and_versions() {
    let text = "version: \"2\"\nnmae: x\nimports: []\nx-editor: {zoom: 2}\ngraph:\n  nodes: [{ id: s, type: start, x-position: [1, 2] }]\n  edgs: []\n";
    let found = errors(text);
    let by_code = |c: &str| found.iter().find(|d| d.code == c).unwrap_or_else(|| panic!("no {c} in {found:?}"));
    assert_eq!(by_code("unsupported-version").path.to_string(), "version");
    assert_eq!(by_code("reserved-field").path.to_string(), "imports");
    let unknown: Vec<(String, Option<String>)> = found
        .iter()
        .filter(|d| d.code == "unknown-field")
        .map(|d| (d.path.to_string(), d.suggestion.clone()))
        .collect();
    assert!(unknown.contains(&("nmae".into(), Some("name".into()))), "{unknown:?}");
    assert!(unknown.contains(&("graph.edgs".into(), Some("edges".into()))), "{unknown:?}");
    assert_eq!(found.len(), 4, "x- keys are ignored: {found:?}");
}

#[test]
fn policies_are_checked() {
    let text = "graph:\n  nodes:\n    - id: s\n      type: start\n      retry: { max_retires: 2 }\n      on_error: { strategy: fail-brnch }\n";
    let found = errors(text);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].path.to_string(), "graph.nodes[0].retry.max_retires");
    assert_eq!(found[0].suggestion.as_deref(), Some("max_retries"));
    assert_eq!(found[1].path.to_string(), "graph.nodes[0].on_error.strategy");
    assert_eq!(found[1].suggestion.as_deref(), Some("fail-branch"));
}

#[test]
fn deprecated_spellings_warn_but_work() {
    let text = r#"
environment_variables: [{ name: K, value_type: string, value: v }]
graph:
  nodes:
    - { id: s, type: start }
    - id: l
      type: loop
      loop_count: 2
      loop_variables: [{ label: n, var_type: number, value_type: constant, value: 0 }]
    - { id: ls, type: loop-start, parentId: l }
  edges:
    - { source: s, target: l, sourceHandle: source }
"#;
    let v = validate_text(text, &Registry::with_builtins());
    assert!(!v.has_errors(), "{:?}", v.diagnostics);
    let deprecated: Vec<(String, Option<String>)> = v
        .diagnostics
        .iter()
        .filter(|d| d.code == "deprecated")
        .map(|d| (d.path.to_string(), d.suggestion.clone()))
        .collect();
    for (path, new) in [
        ("environment_variables", "env"),
        ("environment_variables[0].value_type", "type"),
        ("graph.nodes[1].loop_variables[0].label", "name"),
        ("graph.nodes[1].loop_variables[0].var_type", "type"),
        ("graph.nodes[1].loop_variables[0].value_type", "input_type"),
        ("graph.nodes[2].parentId", "parent"),
        ("graph.edges[0].sourceHandle", "handle"),
    ] {
        assert!(deprecated.contains(&(path.to_owned(), Some(new.to_owned()))), "{path}: {deprecated:?}");
    }
}

#[test]
fn import_layout_is_lenient() {
    let text = r#"{"app": {"name": "x"}, "kind": "app", "version": "0.1.5", "workflow": {"graph": {
        "nodes": [{"id": "s", "type": "custom", "data": {"type": "start", "variables": [], "selected": false, "foo": 1}}],
        "edges": [], "viewport": {"x": 0}}}}"#;
    let v = validate_text(text, &Registry::with_builtins());
    assert!(!v.has_errors(), "{:?}", v.diagnostics);
}

#[test]
fn syntax_errors_have_positions() {
    let found = errors("graph:\n  nodes: [\n");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].code, "syntax");
    assert!(found[0].span.is_some());
    let found = errors("{\"graph\": {\n  \"nodes\": [,]}}");
    assert_eq!(found[0].span.map(|s| s.line), Some(2));
}

#[test]
fn config_type_errors_point_at_the_field() {
    let text = "graph:\n  nodes:\n    - id: s\n      type: start\n      variables:\n        - { variable: a, required: maybe }\n";
    let found = errors(text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path.to_string(), "graph.nodes[0].variables[0].required");
    assert_eq!(found[0].span.map(|s| s.line), Some(6));
}

#[test]
fn http_request_timeout_is_node_config_not_policy() {
    // `timeout` used to be swallowed as a policy key, silently dropping http-request timeouts.
    let wf = Workflow::parse("graph:\n  nodes:\n    - { id: h, type: http-request, url: x, timeout: { read: 5 } }\n")
        .unwrap();
    assert_eq!(wf.graph.nodes[0].config.get("timeout"), Some(&serde_json::json!({"read": 5})));
}

const APPROVAL: &str = r#"version: "1"
graph:
  nodes:
    - id: start
      type: start
    - id: ask
      type: human-input
      prompt: "Send it?"
      actions: [{ id: send }, { id: skip }]
      expire_after_ms: 3600000
      on_expire: { action: skip }
    - id: end
      type: end
  edges:
    - { source: start, target: ask }
    - { source: ask, target: end, handle: send }
    - { source: ask, target: end, handle: skip }
"#;

#[test]
fn human_input_on_expire_is_a_known_field() {
    let v = validate_text(APPROVAL, &Registry::with_builtins());
    assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
    assert!(v.graph.is_some());

    let stop = APPROVAL.replace("on_expire: { action: skip }", "on_expire: stop");
    assert!(errors(&stop).is_empty(), "{:?}", errors(&stop));
}

#[test]
fn human_input_on_expire_must_name_one_of_its_actions() {
    let text = APPROVAL.replace("on_expire: { action: skip }", "on_expire: { action: publish }");
    let found = errors(&text);
    assert!(found.iter().any(|d| d.message.contains("`publish`")), "{found:?}");
}
