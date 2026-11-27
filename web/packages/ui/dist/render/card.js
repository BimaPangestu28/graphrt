/**
 * `adaptive_card` blocks: the subset of Adaptive Cards 1.0 to 1.6 that the
 * validator accepts, with inputs and actions.
 *
 * `Action.Submit` and `Action.Execute` collect the card's inputs and hand them
 * to the host (`RenderContext.onAction`). `Action.OpenUrl` is a plain link,
 * `Action.ShowCard` and `Action.ToggleVisibility` act inside the card.
 * Templating, media and anything the validator refuses are not drawn.
 */
import { el } from "../dom.js";
import { isSupportedAction, isSupportedElement } from "../validate/adaptive_card.js";
import { isImageUrl, isLinkUrl, isObject } from "../validate/checks.js";
import { frame, text } from "./common.js";
let nextId = 0;
function freshId() {
    nextId += 1;
    return `grt-${nextId}`;
}
function lower(fields, key) {
    return text(fields, key)?.toLowerCase();
}
/**
 * The small part of Markdown that card texts use: `**bold**`, `_italic_`
 * and `[label](https://link)`. Everything else stays text.
 */
export function inlineMarkdown(source) {
    const nodes = [];
    const pattern = /\*\*([^*]+)\*\*|_([^_]+)_|\[([^\]]+)\]\(([^)\s]+)\)/g;
    let last = 0;
    for (const match of source.matchAll(pattern)) {
        const at = match.index;
        if (at > last) {
            nodes.push(source.slice(last, at));
        }
        const [whole, bold, italic, label, url] = match;
        if (bold !== undefined) {
            nodes.push(el("strong", {}, bold));
        }
        else if (italic !== undefined) {
            nodes.push(el("em", {}, italic));
        }
        else if (label !== undefined && url !== undefined && isLinkUrl(url)) {
            nodes.push(el("a", { href: url, target: "_blank", rel: "noopener noreferrer" }, label));
        }
        else {
            nodes.push(whole);
        }
        last = at + whole.length;
    }
    if (last < source.length) {
        nodes.push(source.slice(last));
    }
    return nodes;
}
function textClasses(fields) {
    const classes = [];
    const size = lower(fields, "size");
    if (size !== undefined && ["small", "medium", "large", "extralarge"].includes(size)) {
        classes.push(`ac-${size}`);
    }
    const weight = lower(fields, "weight");
    if (weight === "lighter" || weight === "bolder") {
        classes.push(`ac-${weight}`);
    }
    const colour = lower(fields, "color");
    if (colour !== undefined && ["accent", "good", "warning", "attention"].includes(colour)) {
        classes.push(`ac-${colour}`);
    }
    if (fields.isSubtle === true) {
        classes.push("ac-subtle");
    }
    if (lower(fields, "fontType") === "monospace") {
        classes.push("ac-mono");
    }
    const align = lower(fields, "horizontalAlignment");
    if (align === "center" || align === "right") {
        classes.push(`ac-${align}`);
    }
    return classes;
}
function image(fields) {
    const url = text(fields, "url");
    // Checked again here: the renderer never trusts that validation ran.
    if (url === undefined || !isImageUrl(url)) {
        return null;
    }
    const size = lower(fields, "size");
    const classes = ["ac-image"];
    if (size !== undefined && ["small", "medium", "large"].includes(size)) {
        classes.push(size);
    }
    if (lower(fields, "style") === "person") {
        classes.push("person");
    }
    return el("img", {
        class: classes.join(" "),
        src: url,
        alt: text(fields, "altText") ?? "",
        loading: "lazy",
        referrerpolicy: "no-referrer",
    });
}
function field(input, control, state, read) {
    const id = freshId();
    control.id = id;
    const label = text(input, "label");
    const required = input.isRequired === true;
    const problem = el("div", { class: "ac-invalid", role: "alert", hidden: "" }, text(input, "errorMessage") ?? "This field is required.");
    const inputId = text(input, "id") ?? "";
    state.inputs.push({
        id: inputId,
        read,
        check: () => {
            const value = read();
            const empty = value === undefined || value === "";
            const native = control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement
                ? control.validity.valid
                : true;
            const ok = !(required && empty) && native;
            problem.hidden = ok;
            control.setAttribute("aria-invalid", ok ? "false" : "true");
            return ok;
        },
    });
    return el("div", { class: "ac-field" }, label !== undefined && el("label", { for: id, class: required ? "ac-required" : undefined }, label), control, problem);
}
function scalarText(value) {
    return typeof value === "string" || typeof value === "number" ? String(value) : undefined;
}
function inputElement(input, type, state) {
    const initial = scalarText(input.value);
    const placeholder = text(input, "placeholder");
    if (type === "Input.Toggle") {
        const on = text(input, "valueOn") ?? "true";
        const off = text(input, "valueOff") ?? "false";
        const box = el("input", { type: "checkbox" });
        box.checked = initial === on;
        const id = freshId();
        box.id = id;
        state.inputs.push({
            id: text(input, "id") ?? "",
            read: () => (box.checked ? on : off),
            check: () => true,
        });
        return el("div", { class: "ac-choice" }, box, el("label", { for: id }, text(input, "title") ?? text(input, "label") ?? ""));
    }
    if (type === "Input.ChoiceSet") {
        const choices = (Array.isArray(input.choices) ? input.choices : []).filter(isObject);
        const multi = input.isMultiSelect === true;
        const selected = new Set((initial ?? "").split(",").filter((value) => value !== ""));
        if (!multi && lower(input, "style") !== "expanded") {
            const select = el("select", {});
            if (placeholder !== undefined || !choices.some((choice) => selected.has(text(choice, "value") ?? ""))) {
                select.append(el("option", { value: "" }, placeholder ?? ""));
            }
            for (const choice of choices) {
                const option = el("option", { value: text(choice, "value") ?? "" }, text(choice, "title") ?? "");
                option.selected = selected.has(option.value);
                select.append(option);
            }
            return field(input, select, state, () => (select.value === "" ? undefined : select.value));
        }
        const group = freshId();
        const boxes = choices.map((choice) => {
            const box = el("input", {
                type: multi ? "checkbox" : "radio",
                name: group,
                value: text(choice, "value") ?? "",
            });
            box.checked = selected.has(box.value);
            box.id = freshId();
            return {
                box,
                row: el("div", { class: "ac-choice" }, box, el("label", { for: box.id }, text(choice, "title") ?? "")),
            };
        });
        const set = el("fieldset", { class: "ac-choices" }, ...boxes.map((item) => item.row));
        const read = () => {
            const values = boxes.filter((item) => item.box.checked).map((item) => item.box.value);
            return values.length === 0 ? undefined : values.join(",");
        };
        return field(input, set, state, read);
    }
    if (type === "Input.Text" && input.isMultiline === true) {
        const area = el("textarea", { placeholder, rows: "3" });
        area.value = initial ?? "";
        if (typeof input.maxLength === "number") {
            area.maxLength = input.maxLength;
        }
        return field(input, area, state, () => area.value);
    }
    const kinds = {
        "Input.Number": "number",
        "Input.Date": "date",
        "Input.Time": "time",
    };
    const styles = ["email", "tel", "url", "password"];
    const style = lower(input, "style");
    const control = el("input", {
        type: kinds[type] ?? (style !== undefined && styles.includes(style) ? style : "text"),
        placeholder,
        min: scalarText(input.min),
        max: scalarText(input.max),
    });
    control.value = initial ?? "";
    if (type === "Input.Text") {
        if (typeof input.maxLength === "number") {
            control.maxLength = input.maxLength;
        }
        const regex = text(input, "regex");
        if (regex !== undefined) {
            control.pattern = regex;
        }
    }
    if (type === "Input.Number") {
        control.step = "any";
        control.inputMode = "decimal";
        return field(input, control, state, () => (control.value === "" ? undefined : Number(control.value)));
    }
    return field(input, control, state, () => control.value);
}
/** Collect the inputs and hand the action to the host, unless an input is not acceptable. */
function send(action, type, state) {
    const data = {};
    if (lower(action, "associatedInputs") !== "none") {
        const checks = state.inputs.map((input) => input.check());
        if (checks.includes(false)) {
            return;
        }
        for (const input of state.inputs) {
            const value = input.read();
            if (value !== undefined) {
                data[input.id] = value;
            }
        }
    }
    if (isObject(action.data)) {
        Object.assign(data, action.data);
    }
    else if (typeof action.data === "string") {
        data.data = action.data;
    }
    for (const button of state.senders) {
        button.disabled = true;
    }
    const detail = { type, id: text(action, "id"), verb: text(action, "verb"), data };
    state.context.onAction(detail);
}
/** What pressing `action` does, or `undefined` for one that does nothing here. */
function runner(action, state, showCards) {
    switch (text(action, "type")) {
        case "Action.Submit":
            return () => send(action, "submit", state);
        case "Action.Execute":
            return () => send(action, "execute", state);
        case "Action.ToggleVisibility":
            return () => {
                for (const target of Array.isArray(action.targetElements) ? action.targetElements : []) {
                    const id = typeof target === "string" ? target : isObject(target) ? text(target, "elementId") : undefined;
                    const element = id === undefined ? undefined : state.byId.get(id);
                    if (element !== undefined) {
                        const wanted = isObject(target) && typeof target.isVisible === "boolean" ? target.isVisible : element.hidden;
                        element.hidden = !wanted;
                    }
                }
            };
        case "Action.ShowCard": {
            if (!isObject(action.card)) {
                return undefined;
            }
            const inner = el("div", { class: "ac-showcard card", hidden: "" }, ...cardContent(action.card, state));
            showCards.append(inner);
            return () => {
                inner.hidden = !inner.hidden;
            };
        }
        default:
            return undefined;
    }
}
/** The action to draw: `action` itself, its fallback when it is not supported, or nothing. */
function supportedAction(action) {
    if (!isObject(action)) {
        return undefined;
    }
    if (isSupportedAction(text(action, "type") ?? "")) {
        return action;
    }
    return supportedAction(action.fallback);
}
function actionButton(value, state, showCards) {
    const action = supportedAction(value);
    if (action === undefined) {
        return null;
    }
    const title = text(action, "title") ?? "";
    const iconUrl = text(action, "iconUrl");
    const icon = iconUrl !== undefined && isImageUrl(iconUrl)
        ? el("img", { src: iconUrl, alt: "", referrerpolicy: "no-referrer" })
        : null;
    const tooltip = text(action, "tooltip");
    if (text(action, "type") === "Action.OpenUrl") {
        const url = text(action, "url");
        if (url === undefined || !isLinkUrl(url)) {
            return null;
        }
        return el("a", { class: "button", href: url, target: "_blank", rel: "noopener noreferrer", title: tooltip }, icon, title);
    }
    const style = lower(action, "style");
    const button = el("button", {
        type: "button",
        class: style === "positive" || style === "destructive" ? style : undefined,
        title: tooltip,
    }, icon, title);
    button.disabled = action.isEnabled === false;
    const run = runner(action, state, showCards);
    if (run !== undefined) {
        button.addEventListener("click", run);
    }
    const type = text(action, "type");
    if (type === "Action.Submit" || type === "Action.Execute") {
        state.senders.push(button);
    }
    return button;
}
function actionSet(actions, state) {
    const list = Array.isArray(actions) ? actions : [];
    if (list.length === 0) {
        return null;
    }
    const showCards = el("div", {});
    const buttons = list.map((action) => actionButton(action, state, showCards));
    return el("div", { class: "ac-container" }, el("div", { class: "ac-actions" }, ...buttons), showCards);
}
/** Make `element` press `selectAction` when it is clicked or activated with the keyboard. */
function selectable(element, fields, state) {
    const action = supportedAction(fields.selectAction);
    if (action === undefined) {
        return;
    }
    if (text(action, "type") === "Action.OpenUrl") {
        const url = text(action, "url");
        if (url !== undefined && isLinkUrl(url)) {
            const link = el("a", { href: url, target: "_blank", rel: "noopener noreferrer" });
            link.append(...element.childNodes);
            element.append(link);
        }
        return;
    }
    const run = runner(action, state, element);
    if (run === undefined) {
        return;
    }
    element.classList.add("ac-clickable");
    element.setAttribute("role", "button");
    element.tabIndex = 0;
    element.addEventListener("click", run);
    element.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            run();
        }
    });
}
function elements(items, state) {
    return (Array.isArray(items) ? items : []).map((item) => element(item, state));
}
function element(value, state) {
    if (!isObject(value)) {
        return null;
    }
    const type = text(value, "type") ?? "";
    if (!isSupportedElement(type)) {
        // An element this renderer does not know: its fallback, or nothing.
        return isObject(value.fallback) ? element(value.fallback, state) : null;
    }
    const drawn = draw(value, type, state);
    if (drawn === null) {
        return null;
    }
    if (value.separator === true) {
        drawn.classList.add("ac-separator");
    }
    if (value.isVisible === false) {
        drawn.hidden = true;
    }
    const id = text(value, "id");
    if (id !== undefined && !type.startsWith("Input.")) {
        state.byId.set(id, drawn);
    }
    return drawn;
}
function draw(value, type, state) {
    switch (type) {
        case "TextBlock": {
            const classes = ["ac-text", ...textClasses(value)];
            if (value.wrap === true) {
                classes.push("wrap");
            }
            const heading = lower(value, "style") === "heading";
            if (heading) {
                classes.push("ac-heading");
            }
            return el(heading ? "h4" : "p", { class: classes.join(" ") }, ...inlineMarkdown(text(value, "text") ?? ""));
        }
        case "RichTextBlock": {
            const runs = (Array.isArray(value.inlines) ? value.inlines : []).map((inline) => {
                if (typeof inline === "string") {
                    return inline;
                }
                if (!isObject(inline)) {
                    return null;
                }
                const classes = textClasses(inline);
                for (const style of ["italic", "underline", "strikethrough", "highlight"]) {
                    if (inline[style] === true) {
                        classes.push(`ac-${style}`);
                    }
                }
                const span = el("span", { class: classes.join(" ") }, text(inline, "text") ?? "");
                selectable(span, inline, state);
                return span;
            });
            return el("p", { class: ["ac-text", "wrap", ...textClasses(value)].join(" ") }, ...runs);
        }
        case "Image": {
            const drawn = image(value);
            if (drawn === null) {
                return null;
            }
            const holder = el("div", { class: textClasses(value).join(" ") }, drawn);
            selectable(holder, value, state);
            return holder;
        }
        case "ImageSet":
            return el("div", { class: "ac-images" }, ...(Array.isArray(value.images) ? value.images : []).map((item) => isObject(item) ? image({ size: value.imageSize ?? "medium", ...item }) : null));
        case "Container": {
            const style = lower(value, "style");
            const classes = ["ac-container"];
            if (style !== undefined && style !== "default") {
                classes.push("ac-emphasis");
            }
            const container = el("div", { class: classes.join(" ") }, ...elements(value.items, state));
            selectable(container, value, state);
            return container;
        }
        case "ColumnSet":
            return el("div", { class: "ac-columns" }, ...(Array.isArray(value.columns) ? value.columns : []).map((column) => isObject(column) ? element({ ...column, type: "Column" }, state) : null));
        case "Column": {
            const width = value.width;
            // Weights are classes, not inline styles: a page's style policy stays strict.
            const weight = typeof width === "number" ? Math.min(6, Math.max(1, Math.round(width))) : undefined;
            const sizing = width === "auto" || (typeof width === "string" && width.endsWith("px"))
                ? " auto"
                : weight
                    ? ` w${weight}`
                    : "";
            const column = el("div", { class: `ac-column${sizing}` }, ...elements(value.items, state));
            selectable(column, value, state);
            return column;
        }
        case "FactSet":
            return el("dl", { class: "ac-facts" }, ...(Array.isArray(value.facts) ? value.facts : [])
                .filter(isObject)
                .flatMap((fact) => [
                el("dt", {}, text(fact, "title") ?? ""),
                el("dd", {}, ...inlineMarkdown(text(fact, "value") ?? "")),
            ]));
        case "ActionSet":
            return actionSet(value.actions, state);
        case "Table": {
            const rows = (Array.isArray(value.rows) ? value.rows : []).filter(isObject);
            const header = value.firstRowAsHeader !== false;
            const drawnRows = rows.map((row, r) => el("tr", {}, ...(Array.isArray(row.cells) ? row.cells : [])
                .filter(isObject)
                .map((cell) => el(header && r === 0 ? "th" : "td", {}, el("div", { class: "ac-container" }, ...elements(cell.items, state))))));
            return el("div", { class: "scroll" }, el("table", {}, el("tbody", {}, ...drawnRows)));
        }
        default:
            return type.startsWith("Input.") ? inputElement(value, type, state) : null;
    }
}
function cardContent(card, state) {
    return [...elements(card.body, state), actionSet(card.actions, state)];
}
export function renderCard(fields, context) {
    const state = { context, inputs: [], byId: new Map(), senders: [] };
    const card = isObject(fields.card) ? fields.card : {};
    const body = el("div", { class: "card" }, ...cardContent(card, state));
    if (card.rtl === true) {
        body.dir = "rtl";
    }
    // The block's title repeats the card's own heading in most cards: the card speaks for itself.
    return frame("card-block", { ...fields, title: null, subtitle: null }, body);
}
