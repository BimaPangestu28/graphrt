/**
 * Page context: what the site tells the workflow about the page the visitor is
 * on (the product they look at, the page URL, the cart, a display name). It
 * reaches the run as `sys.context`.
 *
 * It is data from the browser, so anyone can change it. The server checks the
 * same limits as {@link checkChatContext} and never reads it as identity: the
 * signed in user is the verified user token, whatever the context says. Never
 * put secrets or permissions in it.
 */

/** A JSON value. A property that is `undefined` is left out, as `JSON.stringify` does. */
export type ChatContextValue =
  | string
  | number
  | boolean
  | null
  | ChatContextValue[]
  | { [key: string]: ChatContextValue | undefined };

/** The page context: a JSON object. */
export type ChatContext = { [key: string]: ChatContextValue | undefined };

/** The largest context, as UTF-8 JSON. The server refuses a larger one (`400`). */
export const MAX_CONTEXT_BYTES = 8192;

/** The deepest nesting: the context object itself is level 1. */
export const MAX_CONTEXT_DEPTH = 5;

function isPlainObject(value: object): boolean {
  const prototype: unknown = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/** Why `value` is not JSON within the depth limit, or `undefined` when it is. */
function problem(value: unknown, depth: number, path: string): string | undefined {
  if (value === null || typeof value === "string" || typeof value === "boolean") {
    return undefined;
  }
  if (typeof value === "number") {
    return Number.isFinite(value) ? undefined : `${path} is not a finite number`;
  }
  if (typeof value !== "object") {
    return `${path} is ${typeof value}, not JSON`;
  }
  if (depth > MAX_CONTEXT_DEPTH) {
    return `${path} is nested deeper than ${MAX_CONTEXT_DEPTH} levels`;
  }
  if (Array.isArray(value)) {
    for (const [index, item] of value.entries()) {
      const found = problem(item, depth + 1, `${path}[${index}]`);
      if (found !== undefined) {
        return found;
      }
    }
    return undefined;
  }
  if (!isPlainObject(value)) {
    return `${path} is not a plain object`;
  }
  for (const [key, item] of Object.entries(value)) {
    if (item === undefined) {
      // Left out, as `JSON.stringify` does: `{ name: user?.name }` for a visitor without one.
      continue;
    }
    const found = problem(item, depth + 1, `${path}.${key}`);
    if (found !== undefined) {
      return found;
    }
  }
  return undefined;
}

/**
 * Check a page context and return a copy of it, so a later change of the
 * caller's object does not change what is sent.
 *
 * A property whose value is `undefined` is left out.
 *
 * @throws TypeError - When it is not a JSON object, is nested deeper than
 *   {@link MAX_CONTEXT_DEPTH} or is larger than {@link MAX_CONTEXT_BYTES}.
 */
export function checkChatContext(value: unknown): ChatContext {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError("The chat context must be a JSON object");
  }
  const found = problem(value, 1, "context");
  if (found !== undefined) {
    throw new TypeError(`The chat context is not valid: ${found}`);
  }
  const text = JSON.stringify(value);
  const bytes = new TextEncoder().encode(text).length;
  if (bytes > MAX_CONTEXT_BYTES) {
    throw new TypeError(`The chat context is ${bytes} bytes, more than ${MAX_CONTEXT_BYTES}`);
  }
  return JSON.parse(text) as ChatContext;
}
