# Release notes backup

Titles and bodies of every GitHub Release of `Graphrt/graphrt` and
`Graphrt/graphrt-builder-desktop` as they were on 2026-11-26, before the release
notes were rewritten to one format and the Graphrt brand. Tags, assets, dates and
the latest / pre-release flags were not changed.

Each file is a JSON array of `{id, tag_name, name, body, prerelease, draft,
published_at, html_url}`. To restore one release (its `id` is in the same entry):

```sh
jq '.[] | select(.tag_name == "v0.6.2") | {name, body, make_latest: "false"}' \
  docs/release-notes-backup/graphrt.json > /tmp/release.json
gh api -X PATCH repos/Graphrt/graphrt/releases/<id> --input /tmp/release.json
```

Set `make_latest` to `"true"` only for the release that is the latest one.
