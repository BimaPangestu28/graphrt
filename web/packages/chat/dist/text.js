/**
 * Answer text as DOM nodes: paragraphs, code blocks, `inline code`, **bold**
 * and https links. Text is never parsed as HTML.
 */
function inline(source) {
    const nodes = [];
    const pattern = /`([^`\n]+)`|\*\*([^*\n]+)\*\*|\[([^\]\n]+)\]\((https:\/\/[^)\s]+)\)|(https:\/\/[^\s<>()]+[^\s<>().,;:!?'"])/g;
    let last = 0;
    for (const match of source.matchAll(pattern)) {
        if (match.index > last) {
            nodes.push(document.createTextNode(source.slice(last, match.index)));
        }
        const [whole, code, bold, label, url, bare] = match;
        if (code !== undefined) {
            const element = document.createElement("code");
            element.textContent = code;
            nodes.push(element);
        }
        else if (bold !== undefined) {
            const element = document.createElement("strong");
            element.textContent = bold;
            nodes.push(element);
        }
        else {
            const link = document.createElement("a");
            link.href = url ?? bare ?? "";
            link.target = "_blank";
            link.rel = "noopener noreferrer";
            link.textContent = label ?? bare ?? "";
            nodes.push(link);
        }
        last = match.index + whole.length;
    }
    if (last < source.length) {
        nodes.push(document.createTextNode(source.slice(last)));
    }
    return nodes;
}
/**
 * The nodes for an answer. Code fences become `<pre>`, the rest keeps its
 * line breaks.
 *
 * @example
 * element.replaceChildren(...richText("Use `graphrt serve` and **enjoy**."));
 */
export function richText(source) {
    const nodes = [];
    const parts = source.split(/```[^\n]*\n?/);
    parts.forEach((part, i) => {
        if (part === "") {
            return;
        }
        if (i % 2 === 1) {
            const pre = document.createElement("pre");
            pre.textContent = part.replace(/\n$/, "");
            nodes.push(pre);
        }
        else {
            const block = document.createElement("div");
            block.className = "text";
            block.append(...inline(part.replace(/^\n+|\n+$/g, "")));
            nodes.push(block);
        }
    });
    return nodes;
}
