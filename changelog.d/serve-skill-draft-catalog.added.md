- `graphrt serve`: `POST /v1/skills/drafts/model` asks one of the server's
  models to write a skill from a description or a conversation and returns a
  draft (nothing is installed). `POST /v1/skills/catalog/install` installs a
  skill by name from the signed catalog the server was started with: a
  request cannot name an index, a key or an unsigned mode.
- `graphrt serve`: `POST /v1/learning/{agent}/distill` takes `{model}` to
  also ask a model for lessons (always proposals with trust `derived`), and
  `POST /v1/learning/{agent}/skill-draft` writes a skill draft from the
  procedures an agent learned.
- `ServiceOptions::learning_dir` and `ServiceOptions::skills_dir` name the
  stores the served agents read. `build_state` serves `/v1/learning` and
  `/v1/skills` from the same ones.
- `graphrt serve`: a skill draft takes a `title` (`POST /v1/skills/drafts`,
  `POST /v1/skills`), and a draft is accepted as a route returned it, with its
  `findings` and `title`.
