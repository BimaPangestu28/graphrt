- `graphrt plugin install <name>` and `graphrt plugin search` verify the
  minisign signature of the catalog index (`index.json.minisig`) against the
  graphrt-plugins catalog key before the index is parsed, as the desktop app
  does. Before, the CLI trusted any index it could download, so a fake index
  from a mirror, a misconfigured proxy or a replaced release asset could make
  it install any plugin. An index without a signature, with a signature of
  another key, or changed after it was signed is now refused, and tarballs are
  checked against the sha256 of the verified index. Nothing falls back to an
  unverified index. **Breaking** for custom catalogs: an index given with
  `--index` or `$GRAPHRT_PLUGIN_INDEX` needs a valid signature too. A mirror
  serves `index.json.minisig` next to the index, a catalog of another
  publisher names their key with the new `--index-key <KEY|PATH>` (or
  `$GRAPHRT_PLUGIN_INDEX_KEY`), and a local index under development needs the
  new `--allow-unsigned-index`, which is refused for `https://` indexes.
  Installs from a directory or a git URL are unchanged. See
  `docs/plugins.md`, "Catalog signatures".
- A validly signed catalog index that is older than the newest one already
  used from the same place and key is refused as a rollback (state in
  `~/.graphrt/plugin-catalog-state.json`).
- `graphrt-plugin` has a `catalog_signature` module (feature
  `catalog-signature`): the trusted catalog keys and the signature check, with
  no I/O, for hosts that fetch a catalog themselves.
