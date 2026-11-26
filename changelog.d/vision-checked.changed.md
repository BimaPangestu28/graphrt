- `vision` inputs of `llm`, `agent`, `question-classifier` and
  `parameter-extractor` nodes are checked and prepared when the `image`
  feature is compiled in (the default CLI). Images given inline or stored in
  the workspace or `--files-dir` are sent as base64 after the checks, so an
  inline image that is not a PNG, JPEG, GIF or WebP, or is above the limits,
  now fails the node instead of being forwarded to the provider. A stored
  image without a URL, which was an input error before, now works. Images
  given as `https` URLs are passed on unchanged, as before.
