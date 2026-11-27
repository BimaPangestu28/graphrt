/**
 * `adaptive_card` blocks: the subset of Adaptive Cards 1.0 to 1.6 that the
 * validator accepts, with inputs and actions.
 *
 * `Action.Submit` and `Action.Execute` collect the card's inputs and hand them
 * to the host (`RenderContext.onAction`). `Action.OpenUrl` is a plain link,
 * `Action.ShowCard` and `Action.ToggleVisibility` act inside the card.
 * Templating, media and anything the validator refuses are not drawn.
 */
import { type Child } from "../dom.ts";
import { type Fields } from "../validate/checks.ts";
import { type RenderContext } from "./common.ts";
/**
 * The small part of Markdown that card texts use: `**bold**`, `_italic_`
 * and `[label](https://link)`. Everything else stays text.
 */
export declare function inlineMarkdown(source: string): Child[];
export declare function renderCard(fields: Fields, context: RenderContext): HTMLElement;
