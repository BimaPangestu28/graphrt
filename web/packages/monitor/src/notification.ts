/**
 * What the service worker shows for a push message of `graphrt serve`, and
 * where a tap on it leads. Apart from the worker so it can be tested.
 */

/** A notification to show. */
export interface ShownNotification {
  title: string;
  body: string;
  /** One notification per run: a newer one replaces the older. */
  tag: string;
  /** The view of the monitoring page to open, as a URL fragment. */
  open: string;
}

function field(data: unknown, name: string): string | undefined {
  if (typeof data !== "object" || data === null) {
    return undefined;
  }
  const value = (data as Record<string, unknown>)[name];
  return typeof value === "string" && value !== "" ? value : undefined;
}

/**
 * The notification for the payload of a push message (JSON text). A payload
 * that cannot be read still shows something: browsers require a notification
 * for every push.
 */
export function notificationOf(payload: string | undefined): ShownNotification {
  let data: unknown;
  try {
    data = payload === undefined ? undefined : JSON.parse(payload);
  } catch {
    data = undefined;
  }
  const runId = field(data, "run_id");
  const kind = field(data, "kind");
  let open = "#/runs";
  if (runId !== undefined) {
    open = `#/runs/${encodeURIComponent(runId)}`;
  } else if (kind === "test") {
    open = "#/settings";
  }
  return {
    title: field(data, "title") ?? "Your agent needs you",
    body: field(data, "body") ?? "",
    tag: runId ?? kind ?? "graphrt",
    open,
  };
}
