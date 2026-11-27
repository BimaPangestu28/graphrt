- `graphrt serve` now runs a bundle's knowledge-retrieval steps on the
  knowledge config (and providers file) that came in the bundle, as `graphrt
  run` already did. The configs were extracted but the served workflow got
  the host's services, so its steps failed with `no knowledge base
  configured`. Host flags (`--knowledge`, `--providers`) still take
  precedence.
