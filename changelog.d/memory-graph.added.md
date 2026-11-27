- `long_term_memory: {graph: true}` builds a memory graph: after a note is
  saved, a model reads its entities and relations in the background, each
  relation with the period it was true. A newer relation closes the one it
  replaces instead of deleting it, so a recall `as_of` a past time finds what
  was true then. Recall walks the graph two hops wide and fuses the facts
  with the notes. Relations follow their notes (pending with them, gone with
  them), names are cleaned, masked and bounded, and erasing a user removes
  the graph. `graphrt memory graph build|show`, `GET
  /v1/memory/{agent}/graph`. In the default build (cargo feature
  `memory-graph`), not in the slim builds. See "The memory graph" in
  `docs/memory.md`.
