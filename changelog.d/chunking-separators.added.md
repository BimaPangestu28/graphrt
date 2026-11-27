- Configurable chunking in graphrt-rag: `ChunkerConfig` gains `separators`
  (an ordered list of literal strings and, with the new default-on `regex`
  feature, `{ regex: "..." }` patterns that split before each match),
  `min_chars` (merge short pieces into a neighbour when they fit) and
  `split_pages` (never let a chunk span two PDF pages). `ChunkerConfig::validate`
  and `try_chunk_text` reject unusable settings; `KnowledgeConfig` and
  `Ingestor::ingest` now use them, so a bad config fails before anything is
  stored. Chunks carry `start` / `end` character offsets, stored as
  `char_start` / `char_end` metadata, and `estimate_tokens` / `CHARS_PER_TOKEN`
  give the token estimate the chunk sizes are based on. Existing configs keep
  their meaning; the default separators are exported as `DEFAULT_SEPARATORS`.
