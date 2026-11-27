/** A base64url text as the bytes browsers take for `applicationServerKey`. */
export function base64UrlBytes(text) {
    const base64 = text.replaceAll("-", "+").replaceAll("_", "/");
    const binary = atob(base64 + "=".repeat((4 - (base64.length % 4)) % 4));
    const bytes = new Uint8Array(new ArrayBuffer(binary.length));
    for (let i = 0; i < binary.length; i += 1) {
        bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
}
/** The parts of a browser's subscription a server needs; `undefined` when one is missing. */
function keysOf(subscription) {
    const { endpoint, keys } = subscription.toJSON();
    const p256dh = keys?.p256dh;
    const auth = keys?.auth;
    if (endpoint === undefined || p256dh === undefined || auth === undefined) {
        return undefined;
    }
    return { endpoint, keys: { p256dh, auth } };
}
function isAppleMobile() {
    const agent = navigator.userAgent;
    // An iPad says it is a Mac, with a touch screen.
    return /iPhone|iPad|iPod/.test(agent) || (agent.includes("Macintosh") && navigator.maxTouchPoints > 1);
}
function isInstalled() {
    const standalone = navigator.standalone === true;
    return standalone || globalThis.matchMedia?.("(display-mode: standalone)").matches === true;
}
/**
 * The push notifications of this browser.
 *
 * @example
 * const monitor = document.createElement("graphrt-monitor");
 * monitor.push = browserPush();
 */
export function browserPush() {
    const supported = () => "serviceWorker" in navigator && "PushManager" in globalThis && "Notification" in globalThis;
    const manager = async () => (await navigator.serviceWorker.ready).pushManager;
    return {
        supported,
        // Safari on iOS has push only for a page added to the Home Screen.
        needsInstall: () => !supported() && isAppleMobile() && !isInstalled(),
        permission: () => (supported() ? Notification.permission : "default"),
        async subscription() {
            const current = await (await manager()).getSubscription();
            return current === null ? undefined : keysOf(current);
        },
        async subscribe(publicKey) {
            const pushManager = await manager();
            // A subscription made for another server key cannot be reused.
            const applicationServerKey = base64UrlBytes(publicKey);
            let subscription = await pushManager.getSubscription();
            if (subscription !== null) {
                const current = subscription.options.applicationServerKey;
                const same = current !== null &&
                    current.byteLength === applicationServerKey.length &&
                    new Uint8Array(current).every((byte, i) => byte === applicationServerKey[i]);
                if (!same) {
                    await subscription.unsubscribe();
                    subscription = null;
                }
            }
            // Asks the person for permission when they were not asked yet.
            subscription ??= await pushManager.subscribe({ userVisibleOnly: true, applicationServerKey });
            const keys = keysOf(subscription);
            if (keys === undefined) {
                throw new Error("The browser gave a subscription without keys.");
            }
            return keys;
        },
        async unsubscribe() {
            const current = await (await manager()).getSubscription();
            await current?.unsubscribe();
        },
    };
}
