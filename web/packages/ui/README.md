# graphrt-ui (web)

Web components that draw graphrt [UI blocks](../../../docs/workflow-format.md#ui-blocks):
Adaptive Cards, charts, tables, stat tiles and diagrams. No dependencies.

```ts
import { defineGraphrtUi } from "graphrt-ui";

defineGraphrtUi();
const element = document.createElement("graphrt-block");
element.block = { $ui: "stats", version: 1, items: [{ label: "Orders", value: 42 }] };
document.body.append(element);
```

Blocks come from models and tools, so every block is validated before it is
drawn (a port of the `graphrt-ui` crate's validator, tested against the same
[fixtures](../../../crates/graphrt-ui/fixtures)), all text reaches the page as
text, and nothing is loaded from anywhere.

See [docs/web.md](../../../docs/web.md#drawing-ui-blocks).
