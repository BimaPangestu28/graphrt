# graphrt-ui

UI blocks for [graphrt](https://github.com/Graphrt/graphrt): rich visual output that nodes
and tools emit and hosts render — Adaptive Cards, charts, tables, KPI tiles and diagrams.

A block is a JSON object whose `"$ui"` key names its kind:

```json
{"$ui": "stats", "version": 1, "title": "This week",
 "items": [{"label": "Revenue", "value": 125000000, "format": "currency", "currency": "IDR",
            "delta": 12.5, "delta_format": "percent"}]}
```

| kind            | shows                                        |
|-----------------|----------------------------------------------|
| `adaptive_card` | an Adaptive Card 1.0–1.6 with inputs and actions |
| `chart`         | bar, line, area, pie or donut                |
| `table`         | columns and rows                             |
| `stats`         | KPI tiles with a value, change and trend     |
| `diagram`       | a diagram written as Mermaid text            |

- `graphrt_core::ui` finds blocks in node outputs; the `agent` node collects the blocks of
  its tool results in its `ui` output (never sent to the model), and `human-input` shows one
  with its question (`card`).
- `graphrt_ui::validate` is the gate before rendering: strict shapes (unknown fields are
  errors), size limits, and a URL policy (images: `https` or `data:` PNG/JPEG/GIF/WebP;
  links: `https` / `mailto`). Blocks carry no colours, HTML or scripts.
- `diagram` blocks are text only: no HTML (except `<br>`), `click` / `link` statements,
  config directives or colours, at most 20 000 characters. Hosts still render Mermaid with
  `securityLevel: "strict"` and HTML labels off.
- `graphrt_ui::fallback::to_text` writes any block as tidy text for channels that cannot
  draw it (WhatsApp, SMS, e-mail): a card becomes its text plus numbered choices, and
  `match_choice` maps the customer's reply ("2") back to the card's `Action.Submit`; a
  table becomes compact lines, a chart a short summary.
- Feature `png`: `graphrt_ui::png::chart_png` draws a chart as a PNG for channels that
  send media. Pure Rust, with a built-in font subset (DejaVu Sans, see
  `assets/DejaVu-LICENSE.txt`).
- `graphrt_ui::schema(kind)` returns each kind's JSON Schema (also in `schemas/`).
- `fixtures/valid` and `fixtures/invalid` are shared conformance cases for other
  implementations (the `ui-render` catalog plugin, graphrt Desktop's renderer).
