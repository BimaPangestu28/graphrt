/** What every block renderer shares: the frame with title and subtitle. */
import { el } from "../dom.ts";
import type { Fields, Json } from "../validate/checks.ts";

/** What a renderer needs besides the block. */
export interface RenderContext {
  /** A BCP 47 tag for numbers and dates. Default: the browser's. */
  locale: string | undefined;
  /** Called when a person presses a card action that sends data. */
  onAction: (action: BlockAction) => void;
  /** Draws a `diagram` block, or `undefined` to show its source as text. */
  diagram: DiagramRenderer | undefined;
}

/** A card action a person pressed: what the host sends back to the run. */
export interface BlockAction {
  /** `submit` for `Action.Submit`, `execute` for `Action.Execute`. */
  type: "submit" | "execute";
  /** The action's `id`, which a `human-input` node takes as its `action`. */
  id: string | undefined;
  /** The `verb` of an `Action.Execute`. */
  verb: string | undefined;
  /** The card's input values by input id, then the action's own `data` fields. */
  data: Record<string, Json>;
}

/**
 * Draws a diagram from its source (Mermaid text that passed validation).
 * Returns the picture as SVG text, which is shown as an image and never
 * inserted as markup.
 */
export type DiagramRenderer = (source: string, theme: "light" | "dark") => Promise<string>;

export function text(fields: Fields, key: string): string | undefined {
  const value = fields[key];
  return typeof value === "string" && value !== "" ? value : undefined;
}

/** The frame of a block: its title and subtitle above `content`. */
export function frame(kind: string, fields: Fields, ...content: Node[]): HTMLElement {
  const title = text(fields, "title");
  const subtitle = text(fields, "subtitle");
  const head =
    title !== undefined || subtitle !== undefined
      ? el(
          "div",
          { class: "head" },
          title !== undefined && el("h3", { class: "title" }, title),
          subtitle !== undefined && el("p", { class: "subtitle" }, subtitle),
        )
      : null;
  return el("section", { class: `block ${kind}` }, head, ...content);
}
