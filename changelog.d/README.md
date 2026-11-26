# Changelog fragments

Each pull request with a user-visible change adds one file here instead of
editing `CHANGELOG.md`, so pull requests in parallel never conflict on it.

- Name: `<slug>.<kind>.md`, lower case, e.g. `gateway-freeze.added.md`.
  `kind` is a Keep a Changelog section: `added`, `changed`, `deprecated`,
  `removed`, `fixed` or `security`. A breaking change goes in `changed` and
  says so.
- Content: one or more Markdown bullets, written as for `CHANGELOG.md`
  (`- ...`, continuation lines indented by two spaces).

```markdown
- `graphrt gateway`: `pool.idle_mode: freeze` keeps an idle local agent
  frozen and thaws it on the next run.
```

`scripts/changelog.py check` validates the fragments (CI runs it);
`scripts/changelog.py preview` shows the next release's entries. At a release,
`scripts/changelog.py release X.Y.Z` moves `Unreleased` and every fragment into
a `## [X.Y.Z]` section of `CHANGELOG.md` and deletes the fragments (see
CONTRIBUTING.md, "Releasing").
