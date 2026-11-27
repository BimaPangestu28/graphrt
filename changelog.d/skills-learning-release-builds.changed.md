- The release binaries, the Homebrew formula and the container images (agent
  and gateway builds) are built with the `skills` and `learning` cargo
  features, so `graphrt skill`, `graphrt learn`, the `skills` and `learning`
  fields of `agent` nodes and the `/v1/skills` and `/v1/learning` routes work
  without a build from source. `cargo install graphrt-cli` still needs
  `--features skills,learning`, and the slim agent builds stay without them.
  Skills and self learning are no longer marked preview as a whole:
  docs/skills.md and docs/learning.md say what still is (what a model writes,
  and the files of the two stores).
