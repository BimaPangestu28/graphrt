import type { ApprovalRequest } from "@graphrt/client";
import { MonitorSession, type PushBrowser } from "./session.ts";
/** How long ago `ms` was, in a few characters. */
export declare function ago(ms: number, now?: number): string;
/** How long until `ms`, in a few characters; `undefined` once it passed. */
/**
 * What happens at the time limit of `request`, in one sentence; `undefined`
 * for a request without one.
 *
 * @param enforced - Whether the server carries out `on_expire` (`--expire-approvals`).
 *   Without it nothing happens at the limit: the run keeps waiting.
 */
export declare function expiryNote(request: ApprovalRequest, enforced: boolean, now?: number): string | undefined;
export declare function timeLeft(ms: number, now?: number): string | undefined;
type Route = {
    view: "runs" | "approvals" | "settings";
} | {
    view: "run";
    runId: string;
};
declare const BaseElement: typeof HTMLElement;
export declare class GraphrtMonitorElement extends BaseElement {
    #private;
    static readonly observedAttributes: string[];
    /** Push notifications of this browser. Set it before the element is connected. */
    push: PushBrowser | undefined;
    /** A device token from a pairing link. Set it before the element is connected. */
    pairingToken: string | undefined;
    /** The session behind the view, once the element is connected. */
    get session(): MonitorSession | undefined;
    connectedCallback(): void;
    disconnectedCallback(): void;
    attributeChangedCallback(_name: string, _old: string | null, value: string | null): void;
    /** Show a view: the runs, the inbox, the settings, or one run. */
    show(route: Route): void;
}
/** Register `<graphrt-monitor>` (and `<graphrt-block>`). Safe to call more than once. */
export declare function defineGraphrtMonitor(): void;
export {};
