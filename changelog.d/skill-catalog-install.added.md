- `graphrt skill install <name>[@version]` and `graphrt skill search`
  (preview, cargo features `skills` and `plugins`): skills from the `skills`
  list of the catalog's `index-v2.json`. The index is used only after its
  minisign signature verifies, with no unsigned mode, the archive must match
  the sha256 in it and hold exactly the listed skill and version, and the
  normal install then runs. The catalog grants no tool.
