// Example plugin (JavaScript SDK): text transforms plus a human approval step.
//
// In your own plugin, `npm install graphrt-plugin` and import from "graphrt-plugin".
import { Plugin, PluginError, pause } from "../../../sdk/typescript/index.mjs";

const plugin = new Plugin("upper")
  .node("upper.transform", async (ctx, req) => {
    const { text, mode = "upper" } = req.config;
    if (typeof text !== "string") throw new PluginError("`text` must be a string", { kind: "invalid_input" });
    const result = mode === "lower" ? text.toLowerCase() : mode === "title"
      ? text.replace(/\b\w/g, (c) => c.toUpperCase())
      : text.toUpperCase();
    // Stream word by word.
    for (const word of result.split(/(\s+)/)) {
      if (word) ctx.chunk("text", word);
    }
    return { outputs: { text: result, length: result.length } };
  })
  .node("upper.approve", async (ctx, req) => {
    if (!req.resume) {
      return pause("approval", { message: `Approve publishing: ${req.config.text}` }, { text: req.config.text });
    }
    const input = req.resume.input ?? {};
    const approved = input === "yes" || input?.approved === true;
    return {
      outputs: { approved, text: req.resume.state?.text ?? "" },
      branch: approved ? "approved" : "rejected",
    };
  })
  .tool(
    { name: "shout", description: "Uppercase a text", parameters: { type: "object", properties: { text: { type: "string" } } } },
    async (args) => String(args.text ?? "").toUpperCase(),
  );

plugin.run();
