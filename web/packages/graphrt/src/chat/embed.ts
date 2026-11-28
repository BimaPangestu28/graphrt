/**
 * The widget script a server serves at `/chat/embed.js`:
 *
 *   <script src="https://agent.example.com/chat/embed.js" data-workflow="support" async></script>
 *
 * It adds a chat bubble to the page. The server is the one the script came
 * from, which must list the page's origin in `--cors-origin`. Options are
 * `data-` attributes of the script tag or `GraphrtChat.init({...})`, and
 * `window.GraphrtChat` is the API: see `embed-api.ts`.
 */
import { type GraphrtChatHolder, mountEmbed } from "./embed-api.ts";

mountEmbed(
  document.currentScript instanceof HTMLScriptElement ? document.currentScript : null,
  globalThis as GraphrtChatHolder,
);
