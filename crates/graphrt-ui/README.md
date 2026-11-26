# graphrt-ui

UI blocks for [graphrt](https://github.com/Graphrt/graphrt): rich visual output that nodes
and tools emit and hosts render — Adaptive Cards, charts, tables and KPI tiles.

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

- `graphrt_core::ui` finds blocks in node outputs; the `agent` node collects the blocks of
  its tool results in its `ui` output (never sent to the model), and `human-input` shows one
  with its question (`card`).
- `graphrt_ui::validate` is the gate before rendering: strict shapes (unknown fields are
  errors), size limits, and a URL policy (images: `https` or `data:` PNG/JPEG/GIF/WebP;
  links: `https` / `mailto`). Blocks carry no colours, HTML or scripts.
- `graphrt_ui::schema(kind)` returns each kind's JSON Schema (also in `schemas/`).
- `fixtures/valid` and `fixtures/invalid` are shared conformance cases for other
  implementations (the `ui-render` catalog plugin, graphrt Desktop's renderer).
