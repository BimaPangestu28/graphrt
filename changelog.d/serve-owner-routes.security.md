- `graphrt serve` without an API key on an address other hosts can reach
  now answers `403` on `/v1/skills` and `/v1/learning`: anyone who reached
  the port could grant tools to skills and approve lessons, which outlives
  the run. On loopback and with an API key nothing changes.
- Skills of a bundle are recorded as `verified` only when the bundle is
  signed by a key the operator trusts (`--trust-key`,
  `$GRAPHRT_TRUSTED_KEYS`). A bundle signed with any key used to count.
