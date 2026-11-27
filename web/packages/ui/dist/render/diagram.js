/**
 * `diagram` blocks. Without a diagram renderer the source is shown as text,
 * which is always safe. With one (see `mermaidRenderer`) the picture it returns
 * is shown as an image: SVG inside `<img>` runs no scripts and loads nothing.
 */
import { el } from "../dom.js";
import { frame, text } from "./common.js";
function svgDataUrl(svgText) {
    return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svgText)}`;
}
export function renderDiagram(fields, context, theme) {
    const source = text(fields, "source") ?? "";
    const asText = el("pre", {}, source);
    const holder = el("div", { class: "diagram" }, asText);
    const draw = context.diagram;
    if (draw !== undefined) {
        draw(source, theme).then((svgText) => {
            const image = el("img", { alt: text(fields, "fallback_text") ?? text(fields, "title") ?? "Diagram" });
            image.addEventListener("error", () => holder.replaceChildren(asText));
            image.src = svgDataUrl(svgText);
            holder.replaceChildren(image);
        }, () => {
            // A source the drawing library cannot parse stays readable as text.
        });
    }
    return frame("diagram-block", fields, holder);
}
let diagramCounter = 0;
/**
 * A diagram renderer on a Mermaid instance the page loaded itself (this
 * package bundles none). Mermaid is set up for untrusted text: strict security
 * level, no HTML labels, and no setting the diagram could change.
 *
 * @example
 * import mermaid from "mermaid";
 * setDiagramRenderer(mermaidRenderer(mermaid));
 */
export function mermaidRenderer(mermaid) {
    return async (source, theme) => {
        mermaid.initialize({
            startOnLoad: false,
            securityLevel: "strict",
            theme: theme === "dark" ? "dark" : "neutral",
            htmlLabels: false,
            flowchart: { htmlLabels: false },
            secure: [
                "secure",
                "securityLevel",
                "startOnLoad",
                "maxTextSize",
                "htmlLabels",
                "theme",
                "themeVariables",
            ],
        });
        diagramCounter += 1;
        const { svg } = await mermaid.render(`graphrt-diagram-${diagramCounter}`, source);
        return svg;
    };
}
