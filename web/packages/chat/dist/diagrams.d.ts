/** Loads a classic script into the page. Rejects when it cannot be loaded. */
export type ScriptLoader = (url: string) => Promise<void>;
/**
 * Make `diagram` blocks draw as pictures, with the Mermaid that `server`
 * serves. A page that already has Mermaid keeps its own, and no script is
 * loaded. The work is done once per page: later calls get the same answer.
 *
 * @param server - The base URL of the `graphrt serve`, without a trailing slash.
 * @param load - How a script is loaded. Default: a script element.
 * @returns Whether diagrams now draw as pictures. `false` leaves them as text.
 */
export declare function loadDiagrams(server: string, load?: ScriptLoader): Promise<boolean>;
