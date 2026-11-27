- `graphrt-ui`: new block kind `diagram`, a diagram written as Mermaid text
  (`syntax: mermaid`, `source`). `validate` limits it to 20 000 characters and 500
  lines of the common diagram types and rejects HTML (except `<br>`), `click` /
  `link` / `callback` statements, config directives, front matter, colours and
  `javascript:` / `data:` URLs.
- `graphrt-ui`: `fallback::to_text` writes any block as text for channels that
  cannot draw it (WhatsApp, SMS, e-mail). A card becomes its text plus numbered
  choices, and `fallback::match_choice` maps a reply such as "2" back to the
  card's `Action.Submit` with the values the button would send; a table becomes
  compact lines and a chart a short summary. A block's `fallback_text` replaces
  the generated body.
- `graphrt-ui`: feature `png` adds `png::chart_png`, which draws a chart as a PNG
  image (pure Rust, built-in font) for channels that send media.
