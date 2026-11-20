# graphrt-eval

Evaluation harness for graphrt workflows: Promptfoo-shaped eval files and JSONL datasets,
deterministic, trajectory and retrieval assertions, LLM judges written as graphrt workflows (with
calibration against human labels), trials with pass@k / pass^k, baseline comparison with tolerances,
a CI gate, and JSON / JUnit reports.
It powers `graphrt eval` and works from `cargo test` in Rust applications that embed graphrt.

Part of [graphrt](https://github.com/BimaPangestu28/graphrt) — **run your agent everywhere**: one small runtime for
agentic AI workflows on servers, containers, the edge and phones.

```toml
[dev-dependencies]
graphrt-eval = "0.4"
```

```rust,ignore
let suite = graphrt_eval::EvalSuite::load("evals/support.eval.yaml".as_ref())?;
let target = graphrt_eval::EngineTarget::new(engine); // your graphrt_core::Engine
let report = graphrt_eval::Runner::default().run(&suite, &target, |_| {}).await?;
assert!(report.gate_passed(), "{}", graphrt_eval::report::format_summary(&report));
```

- Documentation: <https://docs.rs/graphrt-eval>
- Guide: [Evaluating workflows](https://github.com/BimaPangestu28/graphrt/blob/main/docs/evals.md)
- Project README: <https://github.com/BimaPangestu28/graphrt#readme>

Licensed under the Apache License, Version 2.0.
