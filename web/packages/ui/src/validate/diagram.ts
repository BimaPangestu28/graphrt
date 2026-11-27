/** `diagram` blocks: Mermaid text only. A port of `diagram.rs` of the `graphrt-ui` crate. */
import {
  type Fields,
  MAX_LONG_TEXT,
  MAX_SHORT_TEXT,
  onlyKnown,
  optionalText,
  requiredText,
  UiError,
} from "./checks.ts";

export const SYNTAXES = ["mermaid"];
export const DIAGRAM_TYPES = [
  "flowchart",
  "graph",
  "sequenceDiagram",
  "classDiagram",
  "stateDiagram",
  "stateDiagram-v2",
  "erDiagram",
  "gantt",
  "pie",
  "mindmap",
  "timeline",
  "quadrantChart",
  "gitGraph",
];
export const MAX_SOURCE_CHARS = 20_000;
export const MAX_SOURCE_LINES = 500;

const FIELDS = ["title", "subtitle", "fallback_text", "syntax", "source"];
const INTERACTION_KEYWORDS = ["click", "callback", "link", "links"];
const TYPES_WITH_INTERACTION = [
  "flowchart",
  "graph",
  "sequenceDiagram",
  "classDiagram",
  "stateDiagram",
  "stateDiagram-v2",
  "gantt",
];
const STYLE_KEYWORDS = ["style", "classDef", "linkStyle"];
const TYPES_WITH_STYLE = [
  "flowchart",
  "graph",
  "classDiagram",
  "stateDiagram",
  "stateDiagram-v2",
  "erDiagram",
  "quadrantChart",
];
const URL_SCHEMES = ["javascript:", "vbscript:", "data:"];

/** @throws UiError - When `fields` (a block without its envelope) is not a diagram. */
export function validateDiagram(fields: Fields): void {
  onlyKnown(fields, "", FIELDS);
  optionalText(fields, "title", "", MAX_SHORT_TEXT);
  optionalText(fields, "subtitle", "", MAX_SHORT_TEXT);
  optionalText(fields, "fallback_text", "", MAX_LONG_TEXT);
  const syntax = requiredText(fields, "syntax", "", MAX_SHORT_TEXT);
  if (!SYNTAXES.includes(syntax)) {
    throw new UiError("/syntax", `\`${syntax}\` is not one of: ${SYNTAXES.join(", ")}`);
  }
  const source = requiredText(fields, "source", "", MAX_SOURCE_CHARS);
  const problem = checkSource(source);
  if (problem !== undefined) {
    throw new UiError("/source", problem);
  }
}

/** Lines as Rust's `str::lines`: split at LF, a CR before it dropped, no last empty line. */
function lines(source: string): string[] {
  const parts = source.split("\n").map((line) => (line.endsWith("\r") ? line.slice(0, -1) : line));
  if (parts.at(-1) === "") {
    parts.pop();
  }
  return parts;
}

/**
 * The diagram type a Mermaid source starts with, if it is a known one.
 *
 * @example
 * diagramType("flowchart LR\n  A --> B"); // "flowchart"
 */
export function diagramType(source: string): string | undefined {
  const first = lines(source)
    .map((line) => line.trim())
    .find((line) => line !== "" && !line.startsWith("%%"));
  if (first === undefined) {
    return undefined;
  }
  const word = first.split(/[\s;]/u)[0] ?? "";
  return DIAGRAM_TYPES.find((known) => known === word);
}

/** Why `source` is refused, or `undefined` when it is fine. */
function checkSource(source: string): string | undefined {
  if (source.trim() === "") {
    return "must not be empty";
  }
  if (/[^\n\r\t\P{Cc}]/u.test(source)) {
    return "must not contain control characters";
  }
  const all = lines(source);
  if (all.length > MAX_SOURCE_LINES) {
    return `has ${all.length} lines; at most ${MAX_SOURCE_LINES} are allowed`;
  }
  if (source.trimStart().startsWith("---")) {
    return "front matter (`---`) is not supported: start with the diagram type and put the title in the block's `title`";
  }
  if (source.includes("%%{")) {
    return "directives (`%%{...}%%`) are not supported: the host chooses theme and settings";
  }
  const kind = diagramType(source);
  if (kind === undefined) {
    return `must start with a diagram type, one of: ${DIAGRAM_TYPES.join(", ")}`;
  }
  const lower = source.toLowerCase();
  const scheme = URL_SCHEMES.find((candidate) => lower.includes(candidate));
  if (scheme !== undefined) {
    return `\`${scheme}\` URLs are not allowed in a diagram`;
  }
  for (const [number, line] of all.entries()) {
    const problem = checkLine(line, kind);
    if (problem !== undefined) {
      return `line ${number + 1}: ${problem}`;
    }
  }
  return undefined;
}

function checkLine(line: string, kind: string): string | undefined {
  const statement = line.trim();
  if (statement.startsWith("%%")) {
    return undefined;
  }
  const firstWord = statement.split(/\s/u)[0] ?? "";
  const hasArgument = statement.length > firstWord.length;
  if (hasArgument && TYPES_WITH_INTERACTION.includes(kind) && INTERACTION_KEYWORDS.includes(firstWord)) {
    return `\`${firstWord}\` statements (links and callbacks) are not supported: a diagram is a picture, remove this line`;
  }
  if (hasArgument && TYPES_WITH_STYLE.includes(kind) && STYLE_KEYWORDS.includes(firstWord)) {
    return `\`${firstWord}\` is not supported: the host colours the diagram in its own theme, remove this line`;
  }
  const tag = htmlTag(statement);
  if (tag !== undefined) {
    return `HTML (\`${tag}\`) is not allowed: write plain text, and \`<br>\` for a line break in a label`;
  }
  const shapeStart = statement.indexOf("@{");
  if (shapeStart >= 0 && statement.slice(shapeStart).replace(/\s/gu, "").includes("img:")) {
    return "images in nodes are not supported";
  }
  return undefined;
}

/** The first HTML tag in `line` other than `<br>`, if any. Arrows such as `<--` are not tags. */
function htmlTag(line: string): string | undefined {
  let previous = "";
  for (let start = 0; start < line.length; start += 1) {
    const c = line[start] ?? "";
    const afterAngle = previous === "<";
    previous = c;
    if (c !== "<" || afterAngle) {
      continue;
    }
    const rest = line.slice(start + 1);
    const nameStart = rest.startsWith("/") ? rest.slice(1) : rest;
    if (!/^[A-Za-z]/.test(nameStart)) {
      continue;
    }
    const end = rest.search(/[<>]/);
    if (end < 0 || rest[end] !== ">") {
      continue;
    }
    const inner = rest.slice(0, end);
    const name = /^[A-Za-z0-9]*/.exec(nameStart)?.[0] ?? "";
    const isLineBreak = name.toLowerCase() === "br" && inner.replace(/\/+$/, "").trim().length === name.length;
    if (!isLineBreak) {
      return line.slice(start, start + end + 2);
    }
  }
  return undefined;
}
