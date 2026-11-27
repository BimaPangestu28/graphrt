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
/**
 * The notification for the payload of a push message (JSON text). A payload
 * that cannot be read still shows something: browsers require a notification
 * for every push.
 */
export declare function notificationOf(payload: string | undefined): ShownNotification;
