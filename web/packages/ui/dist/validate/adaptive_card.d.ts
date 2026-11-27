/**
 * `adaptive_card` blocks: an Adaptive Card 1.0 to 1.6 within the subset hosts
 * render. A port of `adaptive_card.rs` of the `graphrt-ui` crate.
 */
import { type Fields } from "./checks.ts";
export declare const MAX_NODES = 400;
export declare const MAX_DEPTH = 12;
export declare const CARD_VERSIONS: string[];
/** The element types this renderer draws. */
export declare function isSupportedElement(elementType: string): boolean;
/** @throws UiError - When `fields` (a block without its envelope) is not an adaptive card block. */
export declare function validateAdaptiveCard(fields: Fields): void;
/** The action types this renderer draws. */
export declare function isSupportedAction(type: string): boolean;
