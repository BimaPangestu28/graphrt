/** `diagram` blocks: Mermaid text only. A port of `diagram.rs` of the `graphrt-ui` crate. */
import { type Fields } from "./checks.ts";
export declare const SYNTAXES: string[];
export declare const DIAGRAM_TYPES: string[];
export declare const MAX_SOURCE_CHARS = 20000;
export declare const MAX_SOURCE_LINES = 500;
/** @throws UiError - When `fields` (a block without its envelope) is not a diagram. */
export declare function validateDiagram(fields: Fields): void;
/**
 * The diagram type a Mermaid source starts with, if it is a known one.
 *
 * @example
 * diagramType("flowchart LR\n  A --> B"); // "flowchart"
 */
export declare function diagramType(source: string): string | undefined;
