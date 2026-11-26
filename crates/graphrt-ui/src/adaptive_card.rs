//! `adaptive_card`: an [Adaptive Card](https://adaptivecards.io) (schema 1.0 to 1.6).
//!
//! ```json
//! {"$ui": "adaptive_card", "version": 1, "title": "Refund request",
//!  "card": {"type": "AdaptiveCard", "version": "1.5",
//!           "body": [{"type": "TextBlock", "text": "Refund **Rp 250.000** to Sari?", "wrap": true},
//!                    {"type": "Input.Text", "id": "note", "label": "Note"}],
//!           "actions": [{"type": "Action.Submit", "id": "approve", "title": "Approve"},
//!                       {"type": "Action.Submit", "id": "reject", "title": "Reject"}]}}
//! ```
//!
//! Supported elements: `TextBlock`, `RichTextBlock` (`TextRun`), `Image`, `ImageSet`,
//! `Container`, `ColumnSet` / `Column`, `FactSet`, `ActionSet`, `Table` (`TableRow`,
//! `TableCell`), `Input.Text`, `Input.Number`, `Input.Date`, `Input.Time`, `Input.Toggle`,
//! `Input.ChoiceSet`. Actions: `Action.Submit`, `Action.Execute`, `Action.OpenUrl`,
//! `Action.ShowCard`, `Action.ToggleVisibility`.
//!
//! Each element and action may only have the properties the Adaptive Cards schema gives
//! it (a typo is an error). Another element type needs a `fallback` (an element or
//! `"drop"`), which hosts show instead. Images must be `https` or `data:` PNG/JPEG/GIF/WebP;
//! `Action.OpenUrl` must be `https` or `mailto`. Template expressions (`${...}`) are not
//! supported: fill in the data before emitting the card. At most 400 elements and
//! actions, nested at most 12 deep; input ids are unique.
//!
//! Hosts answer `Action.Submit` / `Action.Execute` with the action's `verb` or `id` and the
//! card's input values (see the `human-input` node's `card`).

use std::collections::HashSet;

use serde_json::Value as Json;

use crate::UiError;
use crate::checks::{
    Fields, MAX_LONG_TEXT, MAX_SHORT_TEXT, array, check_image_url, check_link_url, child, index, object, only_known,
    optional_choice, optional_text, required_name, required_text,
};

pub const MAX_NODES: usize = 400;
pub const MAX_DEPTH: usize = 12;
/// Adaptive Card schema versions accepted in `card.version`.
pub const CARD_VERSIONS: &[&str] = &["1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6"];

/// Properties every element may have.
const ELEMENT_COMMON: &[&str] = &[
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
const CONTAINER_LIKE: &[&str] = &[
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
const INPUT_COMMON: &[&str] = &["label", "isRequired", "errorMessage", "value", "placeholder"];
const ACTION_COMMON: &[&str] =
    &["type", "id", "title", "iconUrl", "style", "fallback", "tooltip", "isEnabled", "mode", "requires"];
const CARD_FIELDS: &[&str] = &[
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

/// The element types this module checks, with their own properties.
fn element_fields(element_type: &str) -> Option<&'static [&'static str]> {
    Some(match element_type {
        "TextBlock" => &[
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
        "RichTextBlock" => &["inlines", "horizontalAlignment"],
        "Image" => {
            &["url", "altText", "backgroundColor", "horizontalAlignment", "selectAction", "size", "style", "width"]
        }
        "ImageSet" => &["images", "imageSize", "style", "horizontalAlignment"],
        "Container" => &["items"],
        "ColumnSet" => &["columns", "horizontalAlignment"],
        "Column" => &["items", "width"],
        "FactSet" => &["facts"],
        "ActionSet" => &["actions", "horizontalAlignment"],
        "Table" => &[
            "columns",
            "rows",
            "firstRowAsHeader",
            "showGridLines",
            "gridStyle",
            "horizontalCellContentAlignment",
            "verticalCellContentAlignment",
        ],
        "Input.Text" => &["isMultiline", "maxLength", "regex", "style", "inlineAction"],
        "Input.Number" | "Input.Date" | "Input.Time" => &["min", "max"],
        "Input.Toggle" => &["title", "valueOn", "valueOff", "wrap"],
        "Input.ChoiceSet" => &["choices", "isMultiSelect", "style", "wrap"],
        _ => return None,
    })
}

fn uses_container_fields(element_type: &str) -> bool {
    matches!(element_type, "Container" | "ColumnSet" | "Column" | "Table")
}

struct Walk {
    nodes: usize,
    input_ids: HashSet<String>,
}

impl Walk {
    fn enter(&mut self, path: &str, depth: usize) -> Result<(), UiError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(UiError::new(path, format!("the card has more than {MAX_NODES} elements and actions")));
        }
        if depth > MAX_DEPTH {
            return Err(UiError::new(path, format!("the card is nested more than {MAX_DEPTH} levels deep")));
        }
        Ok(())
    }
}

pub(crate) fn validate(fields: &Fields) -> Result<(), UiError> {
    only_known(fields, "", &["title", "fallback_text", "card"])?;
    optional_text(fields, "title", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "fallback_text", "", MAX_LONG_TEXT)?;
    let card = fields.get("card").ok_or_else(|| UiError::new("", "`card` is required"))?;
    let mut walk = Walk { nodes: 0, input_ids: HashSet::new() };
    validate_card(card, "/card", 0, &mut walk)
}

fn validate_card(value: &Json, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    let card = object(value, path)?;
    only_known(card, path, CARD_FIELDS)?;
    if card.get("type").and_then(Json::as_str) != Some("AdaptiveCard") {
        return Err(UiError::new(&child(path, "type"), "must be \"AdaptiveCard\""));
    }
    // Cards inside Action.ShowCard may leave out the version.
    if depth == 0 || card.contains_key("version") {
        let version = required_text(card, "version", path, 8)?;
        if !CARD_VERSIONS.contains(&version) {
            return Err(UiError::new(
                &child(path, "version"),
                format!("`{version}` is not one of: {}", CARD_VERSIONS.join(", ")),
            ));
        }
    }
    optional_text(card, "fallbackText", path, MAX_LONG_TEXT)?;
    optional_text(card, "speak", path, MAX_LONG_TEXT)?;
    check_background_image(card, path)?;
    check_select_action(card, path, depth, walk)?;
    validate_elements(card, "body", path, depth, walk)?;
    validate_actions(card, "actions", path, depth, walk)
}

fn validate_elements(parent: &Fields, key: &str, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    let items_path = child(path, key);
    for (i, element) in array(parent, key, path, 0, MAX_NODES)?.iter().enumerate() {
        validate_element(element, &index(&items_path, i), depth + 1, walk)?;
    }
    Ok(())
}

fn validate_element(value: &Json, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    walk.enter(path, depth)?;
    let element = object(value, path)?;
    let element_type = required_text(element, "type", path, MAX_SHORT_TEXT)?;
    let Some(own_fields) = element_fields(element_type) else {
        return match element.get("fallback") {
            Some(fallback) => validate_fallback(fallback, &child(path, "fallback"), depth, walk),
            None => Err(UiError::new(
                &child(path, "type"),
                format!("element type `{element_type}` is not supported; give it a `fallback` element or \"drop\""),
            )),
        };
    };
    let mut allowed: Vec<&str> = [ELEMENT_COMMON, own_fields].concat();
    if uses_container_fields(element_type) {
        allowed.extend_from_slice(CONTAINER_LIKE);
    }
    if element_type.starts_with("Input.") {
        allowed.extend_from_slice(INPUT_COMMON);
    }
    only_known(element, path, &allowed)?;
    if let Some(fallback) = element.get("fallback") {
        validate_fallback(fallback, &child(path, "fallback"), depth, walk)?;
    }
    if uses_container_fields(element_type) {
        check_background_image(element, path)?;
    }
    if element_fields(element_type).is_some_and(|f| f.contains(&"selectAction")) || uses_container_fields(element_type)
    {
        check_select_action(element, path, depth, walk)?;
    }
    match element_type {
        "TextBlock" => {
            card_text(element, "text", path, true)?;
            optional_choice(element, "style", path, &["default", "heading", "columnHeader"])?;
        }
        "RichTextBlock" => validate_inlines(element, path, depth, walk)?,
        "Image" => validate_image(element, path)?,
        "ImageSet" => {
            for (i, image) in array(element, "images", path, 1, 100)?.iter().enumerate() {
                let image_path = index(&child(path, "images"), i);
                walk.enter(&image_path, depth + 1)?;
                let image = object(image, &image_path)?;
                only_known(
                    image,
                    &image_path,
                    &[ELEMENT_COMMON, element_fields("Image").unwrap_or_default()].concat(),
                )?;
                validate_image(image, &image_path)?;
            }
        }
        "Container" | "Column" => validate_elements(element, "items", path, depth, walk)?,
        "ColumnSet" => {
            for (i, column) in array(element, "columns", path, 0, 50)?.iter().enumerate() {
                let column_path = index(&child(path, "columns"), i);
                let column_type = column.get("type").and_then(Json::as_str).unwrap_or("Column");
                if column_type != "Column" {
                    return Err(UiError::new(&child(&column_path, "type"), "a ColumnSet holds only Column elements"));
                }
                let with_type = with_default_type(column, "Column");
                validate_element(&with_type, &column_path, depth + 1, walk)?;
            }
        }
        "FactSet" => {
            for (i, fact) in array(element, "facts", path, 1, 100)?.iter().enumerate() {
                let fact_path = index(&child(path, "facts"), i);
                let fact = object(fact, &fact_path)?;
                only_known(fact, &fact_path, &["title", "value"])?;
                card_text(fact, "title", &fact_path, false)?;
                card_text(fact, "value", &fact_path, true)?;
            }
        }
        "ActionSet" => validate_actions(element, "actions", path, depth, walk)?,
        "Table" => validate_table(element, path, depth, walk)?,
        input if input.starts_with("Input.") => validate_input(element, input, path, depth, walk)?,
        _ => {}
    }
    Ok(())
}

fn with_default_type(value: &Json, element_type: &str) -> Json {
    let mut owned = value.clone();
    if let Some(map) = owned.as_object_mut() {
        map.entry("type").or_insert_with(|| Json::from(element_type));
    }
    owned
}

fn validate_fallback(value: &Json, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    match value {
        Json::String(s) if s == "drop" => Ok(()),
        Json::Object(_) => validate_element(value, path, depth, walk),
        _ => Err(UiError::new(path, "must be an element or \"drop\"")),
    }
}

/// Text shown on the card: no template expressions, bounded length.
fn card_text(fields: &Fields, key: &str, path: &str, long: bool) -> Result<(), UiError> {
    let text = required_text(fields, key, path, if long { MAX_LONG_TEXT } else { MAX_SHORT_TEXT })?;
    if text.contains("${") {
        return Err(UiError::new(
            &child(path, key),
            "template expressions (${...}) are not supported; put the values into the text",
        ));
    }
    Ok(())
}

fn validate_inlines(element: &Fields, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    const TEXT_RUN: &[&str] = &[
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
    for (i, inline) in array(element, "inlines", path, 1, 200)?.iter().enumerate() {
        let inline_path = index(&child(path, "inlines"), i);
        walk.enter(&inline_path, depth + 1)?;
        if inline.is_string() {
            continue;
        }
        let run = object(inline, &inline_path)?;
        if run.get("type").and_then(Json::as_str) != Some("TextRun") {
            return Err(UiError::new(&child(&inline_path, "type"), "inlines are strings or TextRun objects"));
        }
        only_known(run, &inline_path, TEXT_RUN)?;
        card_text(run, "text", &inline_path, true)?;
        check_select_action(run, &inline_path, depth + 1, walk)?;
    }
    Ok(())
}

fn validate_image(image: &Fields, path: &str) -> Result<(), UiError> {
    let url = required_text(image, "url", path, crate::checks::MAX_URL)?;
    check_image_url(url, &child(path, "url"))?;
    optional_text(image, "altText", path, MAX_SHORT_TEXT)?;
    Ok(())
}

fn check_background_image(fields: &Fields, path: &str) -> Result<(), UiError> {
    let image_path = child(path, "backgroundImage");
    match fields.get("backgroundImage") {
        None | Some(Json::Null) => Ok(()),
        Some(Json::String(url)) => check_image_url(url, &image_path),
        Some(Json::Object(image)) => {
            only_known(image, &image_path, &["url", "fillMode", "horizontalAlignment", "verticalAlignment"])?;
            let url = required_text(image, "url", &image_path, crate::checks::MAX_URL)?;
            check_image_url(url, &child(&image_path, "url"))
        }
        Some(_) => Err(UiError::new(&image_path, "must be a URL or an object with `url`")),
    }
}

fn check_select_action(fields: &Fields, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    let Some(action) = fields.get("selectAction") else { return Ok(()) };
    let action_path = child(path, "selectAction");
    if action.get("type").and_then(Json::as_str) == Some("Action.ShowCard") {
        return Err(UiError::new(&action_path, "a selectAction cannot be Action.ShowCard"));
    }
    validate_action(action, &action_path, depth + 1, walk)
}

fn validate_table(table: &Fields, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    const COLUMN: &[&str] = &["type", "width", "horizontalCellContentAlignment", "verticalCellContentAlignment"];
    const ROW: &[&str] = &["type", "cells", "style", "horizontalCellContentAlignment", "verticalCellContentAlignment"];
    let columns = array(table, "columns", path, 1, 30)?;
    for (i, column) in columns.iter().enumerate() {
        let column_path = index(&child(path, "columns"), i);
        only_known(object(column, &column_path)?, &column_path, COLUMN)?;
    }
    for (r, row) in array(table, "rows", path, 0, 200)?.iter().enumerate() {
        let row_path = index(&child(path, "rows"), r);
        walk.enter(&row_path, depth + 1)?;
        let row = object(row, &row_path)?;
        only_known(row, &row_path, ROW)?;
        let cells = array(row, "cells", &row_path, 0, columns.len())?;
        for (c, cell) in cells.iter().enumerate() {
            let cell_path = index(&child(&row_path, "cells"), c);
            walk.enter(&cell_path, depth + 2)?;
            let cell = object(cell, &cell_path)?;
            let mut allowed = vec!["type", "items"];
            allowed.extend_from_slice(CONTAINER_LIKE);
            only_known(cell, &cell_path, &allowed)?;
            check_background_image(cell, &cell_path)?;
            check_select_action(cell, &cell_path, depth + 2, walk)?;
            validate_elements(cell, "items", &cell_path, depth + 2, walk)?;
        }
    }
    Ok(())
}

fn validate_input(input: &Fields, input_type: &str, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    let id = required_name(input, "id", path)?;
    if !walk.input_ids.insert(id.to_owned()) {
        return Err(UiError::new(&child(path, "id"), format!("input id `{id}` is used twice")));
    }
    optional_text(input, "label", path, MAX_SHORT_TEXT)?;
    optional_text(input, "placeholder", path, MAX_SHORT_TEXT)?;
    optional_text(input, "errorMessage", path, MAX_SHORT_TEXT)?;
    match input_type {
        "Input.Toggle" => {
            optional_text(input, "title", path, MAX_SHORT_TEXT)?;
        }
        "Input.ChoiceSet" => {
            optional_choice(input, "style", path, &["compact", "expanded", "filtered"])?;
            for (i, choice) in array(input, "choices", path, 1, 200)?.iter().enumerate() {
                let choice_path = index(&child(path, "choices"), i);
                let choice = object(choice, &choice_path)?;
                only_known(choice, &choice_path, &["title", "value"])?;
                card_text(choice, "title", &choice_path, false)?;
                required_text(choice, "value", &choice_path, MAX_SHORT_TEXT)?;
            }
        }
        "Input.Text" => {
            let Some(action) = input.get("inlineAction") else { return Ok(()) };
            let action_path = child(path, "inlineAction");
            if action.get("type").and_then(Json::as_str) == Some("Action.ShowCard") {
                return Err(UiError::new(&action_path, "an inlineAction cannot be Action.ShowCard"));
            }
            validate_action(action, &action_path, depth + 1, walk)?;
        }
        _ => {}
    }
    Ok(())
}

fn validate_actions(parent: &Fields, key: &str, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    let actions_path = child(path, key);
    for (i, action) in array(parent, key, path, 0, 20)?.iter().enumerate() {
        validate_action(action, &index(&actions_path, i), depth + 1, walk)?;
    }
    Ok(())
}

fn validate_action(value: &Json, path: &str, depth: usize, walk: &mut Walk) -> Result<(), UiError> {
    walk.enter(path, depth)?;
    let action = object(value, path)?;
    let action_type = required_text(action, "type", path, MAX_SHORT_TEXT)?;
    let own: &[&str] = match action_type {
        "Action.Submit" => &["data", "associatedInputs"],
        "Action.Execute" => &["verb", "data", "associatedInputs"],
        "Action.OpenUrl" => &["url"],
        "Action.ShowCard" => &["card"],
        "Action.ToggleVisibility" => &["targetElements"],
        other => {
            return match action.get("fallback") {
                Some(Json::String(s)) if s == "drop" => Ok(()),
                Some(fallback @ Json::Object(_)) => validate_action(fallback, &child(path, "fallback"), depth, walk),
                _ => Err(UiError::new(
                    &child(path, "type"),
                    format!("action type `{other}` is not supported; give it a `fallback` action or \"drop\""),
                )),
            };
        }
    };
    only_known(action, path, &[ACTION_COMMON, own].concat())?;
    optional_text(action, "title", path, MAX_SHORT_TEXT)?;
    optional_text(action, "tooltip", path, MAX_SHORT_TEXT)?;
    optional_choice(action, "style", path, &["default", "positive", "destructive"])?;
    optional_choice(action, "associatedInputs", path, &["auto", "none"])?;
    if let Some(icon) = optional_text(action, "iconUrl", path, crate::checks::MAX_URL)? {
        check_image_url(icon, &child(path, "iconUrl"))?;
    }
    let fallback_ok = action.get("fallback").is_none_or(|fallback| fallback == "drop" || fallback.is_object());
    if !fallback_ok {
        return Err(UiError::new(&child(path, "fallback"), "must be an action or \"drop\""));
    }
    match action_type {
        "Action.OpenUrl" => check_link_url(required_text(action, "url", path, 2048)?, &child(path, "url"))?,
        "Action.ShowCard" => {
            let card = action.get("card").ok_or_else(|| UiError::new(path, "`card` is required"))?;
            validate_card(card, &child(path, "card"), depth + 1, walk)?;
        }
        "Action.Execute" => {
            optional_text(action, "verb", path, MAX_SHORT_TEXT)?;
        }
        "Action.ToggleVisibility" => {
            for (i, target) in array(action, "targetElements", path, 1, 50)?.iter().enumerate() {
                let target_path = index(&child(path, "targetElements"), i);
                if !target.is_string() {
                    let target = object(target, &target_path)?;
                    only_known(target, &target_path, &["elementId", "isVisible"])?;
                    required_name(target, "elementId", &target_path)?;
                }
            }
        }
        _ => {}
    }
    match action.get("data") {
        None | Some(Json::Null | Json::String(_) | Json::Object(_)) => Ok(()),
        Some(_) => Err(UiError::new(&child(path, "data"), "must be an object or a string")),
    }
}

#[cfg(test)]
mod tests {
    use crate::validate;
    use serde_json::{Value, json};

    fn card(body: Value, actions: Value) -> Result<(), crate::UiError> {
        validate(&json!({
            "$ui": "adaptive_card", "version": 1,
            "card": {"type": "AdaptiveCard", "version": "1.5", "body": body, "actions": actions},
        }))
        .map(|_| ())
    }

    #[test]
    fn accepts_a_typical_approval_card() {
        let body = json!([
            {"type": "TextBlock", "text": "Refund **Rp 250.000**?", "wrap": true, "size": "Large", "weight": "Bolder"},
            {"type": "ColumnSet", "columns": [
                {"width": "auto", "items": [{"type": "Image", "url": "data:image/png;base64,iVBORw0KGgo=", "size": "Small"}]},
                {"type": "Column", "width": "stretch", "items": [{"type": "FactSet", "facts": [{"title": "Order", "value": "#1042"}]}]},
            ]},
            {"type": "Input.ChoiceSet", "id": "reason", "label": "Reason", "choices": [{"title": "Damaged", "value": "damaged"}]},
            {"type": "Input.Text", "id": "note", "isMultiline": true},
            {"type": "Media", "sources": [], "fallback": {"type": "TextBlock", "text": "Video not shown"}},
            {"type": "Table", "columns": [{"width": 1}, {"width": 2}], "firstRowAsHeader": true, "rows": [
                {"type": "TableRow", "cells": [{"type": "TableCell", "items": [{"type": "TextBlock", "text": "A"}]}]},
            ]},
        ]);
        let actions = json!([
            {"type": "Action.Submit", "id": "approve", "title": "Approve", "style": "positive", "data": {"refund": true}},
            {"type": "Action.Execute", "verb": "reject", "title": "Reject"},
            {"type": "Action.OpenUrl", "title": "Open order", "url": "https://shop.example/orders/1042"},
            {"type": "Action.ShowCard", "title": "More", "card": {"type": "AdaptiveCard", "body": [{"type": "TextBlock", "text": "x"}]}},
        ]);
        assert_eq!(card(body, actions), Ok(()));
    }

    #[test]
    fn rejects_unsafe_and_unknown_content() {
        let text = |props: Value| {
            let mut element = json!({"type": "TextBlock", "text": "x"});
            element.as_object_mut().unwrap().extend(props.as_object().unwrap().clone());
            card(json!([element]), json!([])).unwrap_err().to_string()
        };
        assert!(text(json!({"onclick": "alert(1)"})).contains("unknown field `onclick`"));
        assert!(text(json!({"text": "Hi ${name}"})).contains("template expressions"));
        let image = |url: &str| card(json!([{"type": "Image", "url": url}]), json!([])).unwrap_err().to_string();
        assert!(image("javascript:alert(1)").contains("images must be"));
        assert!(image("data:image/svg+xml;base64,PHN2Zz4=").contains("images must be"));
        let open = card(json!([]), json!([{"type": "Action.OpenUrl", "url": "javascript:alert(1)"}]));
        assert!(open.unwrap_err().to_string().contains("https or mailto"));
        let media = card(json!([{"type": "Media", "sources": []}]), json!([]));
        assert!(media.unwrap_err().to_string().contains("fallback"));
        let inline = json!([{"type": "Input.Text", "id": "q", "inlineAction": {"type": "Action.OpenUrl", "url": "file:///etc"}}]);
        assert!(card(inline, json!([])).unwrap_err().to_string().contains("https or mailto"));
        let unknown_action = card(json!([]), json!([{"type": "Action.Http", "url": "https://a.example"}]));
        assert!(unknown_action.unwrap_err().to_string().contains("not supported"));
    }

    #[test]
    fn input_ids_are_unique_and_required() {
        let twice = json!([{"type": "Input.Text", "id": "a"}, {"type": "Input.Number", "id": "a"}]);
        assert!(card(twice, json!([])).unwrap_err().message.contains("used twice"));
        assert!(card(json!([{"type": "Input.Text"}]), json!([])).unwrap_err().message.contains("`id` is required"));
    }

    #[test]
    fn limits_depth_and_size() {
        let mut nested = json!({"type": "TextBlock", "text": "deep"});
        for _ in 0..14 {
            nested = json!({"type": "Container", "items": [nested]});
        }
        assert!(card(json!([nested]), json!([])).unwrap_err().message.contains("levels deep"));
        let many: Vec<Value> = (0..401).map(|i| json!({"type": "TextBlock", "text": format!("{i}")})).collect();
        assert!(card(json!(many), json!([])).is_err());
    }

    #[test]
    fn card_envelope_is_checked() {
        let bad_version = validate(
            &json!({"$ui": "adaptive_card", "version": 1, "card": {"type": "AdaptiveCard", "version": "2.0"}}),
        );
        assert!(bad_version.unwrap_err().message.contains("not one of"));
        let not_a_card =
            validate(&json!({"$ui": "adaptive_card", "version": 1, "card": {"type": "Card", "version": "1.5"}}));
        assert!(not_a_card.unwrap_err().message.contains("AdaptiveCard"));
    }
}
