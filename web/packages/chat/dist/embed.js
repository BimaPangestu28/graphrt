import { defineGraphrtChatWidget } from "./widget.js";
const OPTIONS = ["workflow", "heading", "greeting", "placeholder", "theme", "position"];
function mount(script) {
    const holder = globalThis;
    if (holder.GraphrtChat !== undefined || script === null) {
        // Included twice: one bubble is enough.
        return;
    }
    defineGraphrtChatWidget();
    const widget = document.createElement("graphrt-chat-widget");
    // `<server>/chat/embed.js` -> `<server>`, also below a path prefix of a proxy.
    widget.setAttribute("server", new URL("..", script.src).href.replace(/\/$/, ""));
    for (const name of OPTIONS) {
        const value = script.dataset[name];
        if (value !== undefined) {
            widget.setAttribute(name, value);
        }
    }
    holder.GraphrtChat = {
        open: () => widget.open(),
        close: () => widget.close(),
        toggle: () => widget.toggle(),
        setUserToken: (token) => {
            widget.userToken = token;
        },
    };
    const attach = () => {
        document.body.append(widget);
        if (script.dataset.open !== undefined) {
            widget.open();
        }
    };
    if (document.body) {
        attach();
    }
    else {
        document.addEventListener("DOMContentLoaded", attach, { once: true });
    }
}
mount(document.currentScript instanceof HTMLScriptElement ? document.currentScript : null);
