import type { Fields } from "../validate/checks.ts";
import { type RenderContext } from "./common.ts";
export declare function renderDiagram(fields: Fields, context: RenderContext, theme: "light" | "dark"): HTMLElement;
/** The part of the Mermaid API this package uses. */
export interface MermaidLike {
    initialize(config: Record<string, unknown>): void;
    render(id: string, source: string): Promise<{
        svg: string;
    }>;
}
/**
 * A diagram renderer on a Mermaid instance the page loaded itself (this
 * package bundles none). Mermaid is set up for untrusted text: strict security
 * level, no HTML labels, and no setting the diagram could change.
 *
 * @example
 * import mermaid from "mermaid";
 * setDiagramRenderer(mermaidRenderer(mermaid));
 */
export declare function mermaidRenderer(mermaid: MermaidLike): (source: string, theme: "light" | "dark") => Promise<string>;
