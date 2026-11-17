// graphrt plugin `{{name}}`.
//
// Run it in a workflow with:  graphrt run workflow.yaml --plugins <parent dir>
// Never console.log (stdout carries the plugin protocol); use console.error.
import { Plugin, PluginError } from "./graphrt-plugin.mjs";

new Plugin("{{name}}")
  .node("{{name}}.greet", async (ctx, req) => {
    const name = req.config.name;
    if (typeof name !== "string" || !name) {
      throw new PluginError("`name` must be a non-empty string", { kind: "invalid_input" });
    }
    const greeting = `Hello, ${name}!`;
    // Stream word by word; the chunks concatenate to exactly `greeting`.
    greeting.split(" ").forEach((word, i) => ctx.chunk("text", i === 0 ? word : " " + word));
    return { outputs: { text: greeting, length: greeting.length } };
  })
  .run();
