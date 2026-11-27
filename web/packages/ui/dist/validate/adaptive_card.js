/**
 * `adaptive_card` blocks: an Adaptive Card 1.0 to 1.6 within the subset hosts
 * render. A port of `adaptive_card.rs` of the `graphrt-ui` crate.
 */
import { array, checkImageUrl, checkLinkUrl, child, index, isObject, MAX_LONG_TEXT, MAX_SHORT_TEXT, MAX_URL, object, onlyKnown, optionalChoice, optionalText, requiredName, requiredText, UiError, } from "./checks.js";
export const MAX_NODES = 400;
export const MAX_DEPTH = 12;
export const CARD_VERSIONS = ["1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6"];
const ELEMENT_COMMON = [
    "type",
    "id",
    "isVisible",
    "separator",
    "spacing",
    "height",
    "fallback",
    "requires",
    "targetWidth",
    "lang",
    "grid.area",
];
const CONTAINER_LIKE = [
    "selectAction",
    "style",
    "verticalContentAlignment",
    "bleed",
    "backgroundImage",
    "minHeight",
    "rtl",
    "showBorder",
    "roundedCorners",
];
const INPUT_COMMON = ["label", "isRequired", "errorMessage", "value", "placeholder"];
const ACTION_COMMON = [
    "type",
    "id",
    "title",
    "iconUrl",
    "style",
    "fallback",
    "tooltip",
    "isEnabled",
    "mode",
    "requires",
];
const CARD_FIELDS = [
    "type",
    "version",
    "body",
    "actions",
    "selectAction",
    "fallbackText",
    "backgroundImage",
    "minHeight",
    "rtl",
    "speak",
    "lang",
    "verticalContentAlignment",
    "$schema",
    "msteams",
    "metadata",
];
const IMAGE_FIELDS = [
    "url",
    "altText",
    "backgroundColor",
    "horizontalAlignment",
    "selectAction",
    "size",
    "style",
    "width",
];
const INPUT_RANGE = ["min", "max"];
const ELEMENT_FIELDS = {
    TextBlock: [
        "text",
        "color",
        "fontType",
        "horizontalAlignment",
        "isSubtle",
        "maxLines",
        "size",
        "weight",
        "wrap",
        "style",
    ],
    RichTextBlock: ["inlines", "horizontalAlignment"],
    Image: IMAGE_FIELDS,
    ImageSet: ["images", "imageSize", "style", "horizontalAlignment"],
    Container: ["items"],
    ColumnSet: ["columns", "horizontalAlignment"],
    Column: ["items", "width"],
    FactSet: ["facts"],
    ActionSet: ["actions", "horizontalAlignment"],
    Table: [
        "columns",
        "rows",
        "firstRowAsHeader",
        "showGridLines",
        "gridStyle",
        "horizontalCellContentAlignment",
        "verticalCellContentAlignment",
    ],
    "Input.Text": ["isMultiline", "maxLength", "regex", "style", "inlineAction"],
    "Input.Number": INPUT_RANGE,
    "Input.Date": INPUT_RANGE,
    "Input.Time": INPUT_RANGE,
    "Input.Toggle": ["title", "valueOn", "valueOff", "wrap"],
    "Input.ChoiceSet": ["choices", "isMultiSelect", "style", "wrap"],
};
/** The element types this renderer draws. */
export function isSupportedElement(elementType) {
    return Object.hasOwn(ELEMENT_FIELDS, elementType);
}
/** Elements that hold other elements and take the container fields. */
function usesContainerFields(elementType) {
    return ["Container", "ColumnSet", "Column", "Table"].includes(elementType);
}
/** The state of one validation: how many nodes were seen, and the input ids. */
class Walk {
    nodes = 0;
    inputIds = new Set();
    /** @throws UiError - Beyond the node or depth limit. */
    enter(path, depth) {
        this.nodes += 1;
        if (this.nodes > MAX_NODES) {
            throw new UiError(path, `the card has more than ${MAX_NODES} elements and actions`);
        }
        if (depth > MAX_DEPTH) {
            throw new UiError(path, `the card is nested more than ${MAX_DEPTH} levels deep`);
        }
    }
}
/** @throws UiError - When `fields` (a block without its envelope) is not an adaptive card block. */
export function validateAdaptiveCard(fields) {
    onlyKnown(fields, "", ["title", "fallback_text", "card"]);
    optionalText(fields, "title", "", MAX_SHORT_TEXT);
    optionalText(fields, "fallback_text", "", MAX_LONG_TEXT);
    if (fields.card === undefined) {
        throw new UiError("", "`card` is required");
    }
    validateCard(fields.card, "/card", 0, new Walk());
}
function validateCard(value, path, depth, walk) {
    const card = object(value, path);
    onlyKnown(card, path, CARD_FIELDS);
    if (card.type !== "AdaptiveCard") {
        throw new UiError(child(path, "type"), 'must be "AdaptiveCard"');
    }
    // Cards inside Action.ShowCard may leave out the version.
    if (depth === 0 || card.version !== undefined) {
        const version = requiredText(card, "version", path, 8);
        if (!CARD_VERSIONS.includes(version)) {
            throw new UiError(child(path, "version"), `\`${version}\` is not one of: ${CARD_VERSIONS.join(", ")}`);
        }
    }
    optionalText(card, "fallbackText", path, MAX_LONG_TEXT);
    optionalText(card, "speak", path, MAX_LONG_TEXT);
    checkBackgroundImage(card, path);
    checkSelectAction(card, path, depth, walk);
    validateElements(card, "body", path, depth, walk);
    validateActions(card, "actions", path, depth, walk);
}
function validateElements(parent, key, path, depth, walk) {
    const itemsPath = child(path, key);
    array(parent, key, path, 0, MAX_NODES).forEach((element, i) => {
        validateElement(element, index(itemsPath, i), depth + 1, walk);
    });
}
function validateElement(value, path, depth, walk) {
    walk.enter(path, depth);
    const element = object(value, path);
    const elementType = requiredText(element, "type", path, MAX_SHORT_TEXT);
    const ownFields = ELEMENT_FIELDS[elementType];
    if (ownFields === undefined || !isSupportedElement(elementType)) {
        if (element.fallback !== undefined) {
            validateFallback(element.fallback, child(path, "fallback"), depth, walk);
            return;
        }
        throw new UiError(child(path, "type"), `element type \`${elementType}\` is not supported; give it a \`fallback\` element or "drop"`);
    }
    const allowed = [...ELEMENT_COMMON, ...ownFields];
    if (usesContainerFields(elementType)) {
        allowed.push(...CONTAINER_LIKE);
    }
    if (elementType.startsWith("Input.")) {
        allowed.push(...INPUT_COMMON);
    }
    onlyKnown(element, path, allowed);
    if (element.fallback !== undefined) {
        validateFallback(element.fallback, child(path, "fallback"), depth, walk);
    }
    if (usesContainerFields(elementType)) {
        checkBackgroundImage(element, path);
    }
    if (ownFields.includes("selectAction") || usesContainerFields(elementType)) {
        checkSelectAction(element, path, depth, walk);
    }
    switch (elementType) {
        case "TextBlock":
            cardText(element, "text", path, true);
            optionalChoice(element, "style", path, ["default", "heading", "columnHeader"]);
            break;
        case "RichTextBlock":
            validateInlines(element, path, depth, walk);
            break;
        case "Image":
            validateImage(element, path);
            break;
        case "ImageSet":
            array(element, "images", path, 1, 100).forEach((value, i) => {
                const imagePath = index(child(path, "images"), i);
                walk.enter(imagePath, depth + 1);
                const image = object(value, imagePath);
                onlyKnown(image, imagePath, [...ELEMENT_COMMON, ...IMAGE_FIELDS]);
                validateImage(image, imagePath);
            });
            break;
        case "Container":
        case "Column":
            validateElements(element, "items", path, depth, walk);
            break;
        case "ColumnSet":
            array(element, "columns", path, 0, 50).forEach((column, i) => {
                const columnPath = index(child(path, "columns"), i);
                const columnType = isObject(column) && typeof column.type === "string" ? column.type : "Column";
                if (columnType !== "Column") {
                    throw new UiError(child(columnPath, "type"), "a ColumnSet holds only Column elements");
                }
                validateElement(withDefaultType(column, "Column"), columnPath, depth + 1, walk);
            });
            break;
        case "FactSet":
            array(element, "facts", path, 1, 100).forEach((value, i) => {
                const factPath = index(child(path, "facts"), i);
                const fact = object(value, factPath);
                onlyKnown(fact, factPath, ["title", "value"]);
                cardText(fact, "title", factPath, false);
                cardText(fact, "value", factPath, true);
            });
            break;
        case "ActionSet":
            validateActions(element, "actions", path, depth, walk);
            break;
        case "Table":
            validateTable(element, path, depth, walk);
            break;
        default:
            if (elementType.startsWith("Input.")) {
                validateInput(element, elementType, path, depth, walk);
            }
    }
}
/** `value` with `type: elementType` when it is an object without a type. */
function withDefaultType(value, elementType) {
    return isObject(value) && value.type === undefined ? { ...value, type: elementType } : value;
}
function validateFallback(value, path, depth, walk) {
    if (value === "drop") {
        return;
    }
    if (!isObject(value)) {
        throw new UiError(path, 'must be an element or "drop"');
    }
    validateElement(value, path, depth, walk);
}
/** A required text without template expressions, which no host evaluates. */
function cardText(fields, key, path, long) {
    const text = requiredText(fields, key, path, long ? MAX_LONG_TEXT : MAX_SHORT_TEXT);
    if (text.includes("${")) {
        throw new UiError(child(path, key), 
        // biome-ignore lint/suspicious/noTemplateCurlyInString: the message quotes the syntax it refuses
        "template expressions (${...}) are not supported; put the values into the text");
    }
}
const TEXT_RUN = [
    "type",
    "text",
    "color",
    "fontType",
    "highlight",
    "isSubtle",
    "italic",
    "selectAction",
    "size",
    "strikethrough",
    "underline",
    "weight",
    "lang",
];
function validateInlines(element, path, depth, walk) {
    array(element, "inlines", path, 1, 200).forEach((inline, i) => {
        const inlinePath = index(child(path, "inlines"), i);
        walk.enter(inlinePath, depth + 1);
        if (typeof inline === "string") {
            return;
        }
        const run = object(inline, inlinePath);
        if (run.type !== "TextRun") {
            throw new UiError(child(inlinePath, "type"), "inlines are strings or TextRun objects");
        }
        onlyKnown(run, inlinePath, TEXT_RUN);
        cardText(run, "text", inlinePath, true);
        checkSelectAction(run, inlinePath, depth + 1, walk);
    });
}
function validateImage(image, path) {
    const url = requiredText(image, "url", path, MAX_URL);
    checkImageUrl(url, child(path, "url"));
    optionalText(image, "altText", path, MAX_SHORT_TEXT);
}
function checkBackgroundImage(fields, path) {
    const imagePath = child(path, "backgroundImage");
    const image = fields.backgroundImage;
    if (image === undefined || image === null) {
        return;
    }
    if (typeof image === "string") {
        checkImageUrl(image, imagePath);
        return;
    }
    if (!isObject(image)) {
        throw new UiError(imagePath, "must be a URL or an object with `url`");
    }
    onlyKnown(image, imagePath, ["url", "fillMode", "horizontalAlignment", "verticalAlignment"]);
    checkImageUrl(requiredText(image, "url", imagePath, MAX_URL), child(imagePath, "url"));
}
function actionType(action) {
    return isObject(action) && typeof action.type === "string" ? action.type : undefined;
}
function checkSelectAction(fields, path, depth, walk) {
    const action = fields.selectAction;
    if (action === undefined) {
        return;
    }
    const actionPath = child(path, "selectAction");
    if (actionType(action) === "Action.ShowCard") {
        throw new UiError(actionPath, "a selectAction cannot be Action.ShowCard");
    }
    validateAction(action, actionPath, depth + 1, walk);
}
const TABLE_COLUMN = ["type", "width", "horizontalCellContentAlignment", "verticalCellContentAlignment"];
const TABLE_ROW = [
    "type",
    "cells",
    "style",
    "horizontalCellContentAlignment",
    "verticalCellContentAlignment",
];
function validateTable(table, path, depth, walk) {
    const columns = array(table, "columns", path, 1, 30);
    columns.forEach((column, i) => {
        const columnPath = index(child(path, "columns"), i);
        onlyKnown(object(column, columnPath), columnPath, TABLE_COLUMN);
    });
    array(table, "rows", path, 0, 200).forEach((value, r) => {
        const rowPath = index(child(path, "rows"), r);
        walk.enter(rowPath, depth + 1);
        const row = object(value, rowPath);
        onlyKnown(row, rowPath, TABLE_ROW);
        array(row, "cells", rowPath, 0, columns.length).forEach((cellValue, c) => {
            const cellPath = index(child(rowPath, "cells"), c);
            walk.enter(cellPath, depth + 2);
            const cell = object(cellValue, cellPath);
            onlyKnown(cell, cellPath, ["type", "items", ...CONTAINER_LIKE]);
            checkBackgroundImage(cell, cellPath);
            checkSelectAction(cell, cellPath, depth + 2, walk);
            validateElements(cell, "items", cellPath, depth + 2, walk);
        });
    });
}
function validateInput(input, inputType, path, depth, walk) {
    const id = requiredName(input, "id", path);
    if (walk.inputIds.has(id)) {
        throw new UiError(child(path, "id"), `input id \`${id}\` is used twice`);
    }
    walk.inputIds.add(id);
    optionalText(input, "label", path, MAX_SHORT_TEXT);
    optionalText(input, "placeholder", path, MAX_SHORT_TEXT);
    optionalText(input, "errorMessage", path, MAX_SHORT_TEXT);
    if (inputType === "Input.Toggle") {
        optionalText(input, "title", path, MAX_SHORT_TEXT);
    }
    else if (inputType === "Input.ChoiceSet") {
        optionalChoice(input, "style", path, ["compact", "expanded", "filtered"]);
        array(input, "choices", path, 1, 200).forEach((value, i) => {
            const choicePath = index(child(path, "choices"), i);
            const choice = object(value, choicePath);
            onlyKnown(choice, choicePath, ["title", "value"]);
            cardText(choice, "title", choicePath, false);
            requiredText(choice, "value", choicePath, MAX_SHORT_TEXT);
        });
    }
    else if (inputType === "Input.Text" && input.inlineAction !== undefined) {
        const actionPath = child(path, "inlineAction");
        if (actionType(input.inlineAction) === "Action.ShowCard") {
            throw new UiError(actionPath, "an inlineAction cannot be Action.ShowCard");
        }
        validateAction(input.inlineAction, actionPath, depth + 1, walk);
    }
}
function validateActions(parent, key, path, depth, walk) {
    const actionsPath = child(path, key);
    array(parent, key, path, 0, 20).forEach((action, i) => {
        validateAction(action, index(actionsPath, i), depth + 1, walk);
    });
}
const ACTION_FIELDS = {
    "Action.Submit": ["data", "associatedInputs"],
    "Action.Execute": ["verb", "data", "associatedInputs"],
    "Action.OpenUrl": ["url"],
    "Action.ShowCard": ["card"],
    "Action.ToggleVisibility": ["targetElements"],
};
/** The action types this renderer draws. */
export function isSupportedAction(type) {
    return Object.hasOwn(ACTION_FIELDS, type);
}
function validateAction(value, path, depth, walk) {
    walk.enter(path, depth);
    const action = object(value, path);
    const type = requiredText(action, "type", path, MAX_SHORT_TEXT);
    const own = ACTION_FIELDS[type];
    if (own === undefined || !isSupportedAction(type)) {
        if (action.fallback === "drop") {
            return;
        }
        if (isObject(action.fallback)) {
            validateAction(action.fallback, child(path, "fallback"), depth, walk);
            return;
        }
        throw new UiError(child(path, "type"), `action type \`${type}\` is not supported; give it a \`fallback\` action or "drop"`);
    }
    onlyKnown(action, path, [...ACTION_COMMON, ...own]);
    optionalText(action, "title", path, MAX_SHORT_TEXT);
    optionalText(action, "tooltip", path, MAX_SHORT_TEXT);
    optionalChoice(action, "style", path, ["default", "positive", "destructive"]);
    optionalChoice(action, "associatedInputs", path, ["auto", "none"]);
    const icon = optionalText(action, "iconUrl", path, MAX_URL);
    if (icon !== undefined) {
        checkImageUrl(icon, child(path, "iconUrl"));
    }
    const fallback = action.fallback;
    if (fallback !== undefined && fallback !== "drop" && !isObject(fallback)) {
        throw new UiError(child(path, "fallback"), 'must be an action or "drop"');
    }
    if (type === "Action.OpenUrl") {
        checkLinkUrl(requiredText(action, "url", path, 2048), child(path, "url"));
    }
    else if (type === "Action.ShowCard") {
        if (action.card === undefined) {
            throw new UiError(path, "`card` is required");
        }
        validateCard(action.card, child(path, "card"), depth + 1, walk);
    }
    else if (type === "Action.Execute") {
        optionalText(action, "verb", path, MAX_SHORT_TEXT);
    }
    else if (type === "Action.ToggleVisibility") {
        array(action, "targetElements", path, 1, 50).forEach((target, i) => {
            const targetPath = index(child(path, "targetElements"), i);
            if (typeof target !== "string") {
                const fields = object(target, targetPath);
                onlyKnown(fields, targetPath, ["elementId", "isVisible"]);
                requiredName(fields, "elementId", targetPath);
            }
        });
    }
    const data = action.data;
    if (data !== undefined && data !== null && typeof data !== "string" && !isObject(data)) {
        throw new UiError(child(path, "data"), "must be an object or a string");
    }
}
