- Bundles: knowledge base files (`knowledge/`) have their own size limit,
  `OpenOptions::max_knowledge_bytes` (default 4 GiB for one file and for all of
  them, `DEFAULT_MAX_KNOWLEDGE_BYTES`), instead of the 64 MiB per file and
  256 MiB in total that other files keep. A passage store of a few hundred
  documents passed 64 MiB, and such a bundle could be neither built nor
  opened. The compression-ratio guard still applies to every file.
