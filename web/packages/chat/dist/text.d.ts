/**
 * Answer text as DOM nodes: paragraphs, code blocks, `inline code`, **bold**
 * and https links. Text is never parsed as HTML.
 */
/**
 * The nodes for an answer. Code fences become `<pre>`, the rest keeps its
 * line breaks.
 *
 * @example
 * element.replaceChildren(...richText("Use `graphrt serve` and **enjoy**."));
 */
export declare function richText(source: string): Node[];
