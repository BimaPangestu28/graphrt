- Token and cost budgets are enforced in every build. The `budgets` feature of
  `graphrt-core` was turned on only for the tests of `graphrt-llm`, so
  `graphrt run`, `graphrt serve`, the gateway's agents, bundles and the mobile
  and desktop libraries ran past `--max-tokens-per-run`, `--max-cost-per-run`
  and a request's `limits` (a 494 token answer under a budget of 50).
  `graphrt-llm` now turns it on for model nodes, and a test drives the real
  binary and server against a model that reports usage.
- A cost budget now refuses a model without a price (`price_missing`) before
  calling it, instead of counting it as free. System One classifiers, which
  have no price, are refused under a cost budget.
