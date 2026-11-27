//! What Slack and Teams share about an Adaptive Card: which cards a channel can
//! show with its own buttons, and the card without the parts those buttons replace.
//!
//! A host answers a card with numbered choices (graphrt-ui `fallback::to_text`):
//! one per submit action, or one per option of the card's single choice input.
//! A channel that draws the card shows those choices as its own buttons, so the
//! tap comes back as the choice's number, the same as a typed reply. What a
//! button cannot carry (a text field, a second input, a multi select) makes the
//! card one for the text fallback instead.

use serde_json::Value as Json;

/// Whether `element` is an input.
fn is_input(element: &Json) -> bool {
    element.get("type").and_then(Json::as_str).is_some_and(|kind| kind.starts_with("Input."))
}

/// Whether `action` sends an answer (and so becomes a choice).
#[cfg_attr(not(feature = "teams"), allow(dead_code))]
pub(crate) fn is_submit(action: &Json) -> bool {
    matches!(action.get("type").and_then(Json::as_str), Some("Action.Submit" | "Action.Execute"))
}

/// The elements of a container-like element: `items`, the items of `columns`, `body`.
fn children(element: &Json) -> Vec<&Json> {
    let mut found = Vec::new();
    for key in ["items", "body"] {
        if let Some(items) = element.get(key).and_then(Json::as_array) {
            found.extend(items.iter());
        }
    }
    if let Some(columns) = element.get("columns").and_then(Json::as_array) {
        for column in columns {
            if let Some(items) = column.get("items").and_then(Json::as_array) {
                found.extend(items.iter());
            }
        }
    }
    found
}

fn inputs<'a>(element: &'a Json, found: &mut Vec<&'a Json>, depth: usize) {
    if depth > 8 {
        return;
    }
    for child in children(element) {
        if is_input(child) {
            found.push(child);
        }
        inputs(child, found, depth + 1);
    }
}

/// Whether buttons can stand for every answer of `card`: no input, or one
/// single choice input whose options are the choices.
pub(crate) fn buttons_can_answer(card: &Json, choices: usize) -> bool {
    let mut found = Vec::new();
    inputs(card, &mut found, 0);
    match found.as_slice() {
        [] => true,
        [only] => {
            let single_choice = only.get("type").and_then(Json::as_str) == Some("Input.ChoiceSet")
                && only.get("isMultiSelect").and_then(Json::as_bool) != Some(true);
            let options = only.get("choices").and_then(Json::as_array).map_or(0, Vec::len);
            single_choice && options == choices && choices > 0
        }
        _ => false,
    }
}

/// `card` without its inputs and submit actions (the channel's buttons replace
/// them), and with every action set left empty removed.
#[cfg_attr(not(feature = "teams"), allow(dead_code))]
pub(crate) fn without_answers(card: &Json) -> Json {
    fn strip(value: &Json, depth: usize) -> Json {
        match value {
            Json::Object(fields) if depth <= 8 => {
                let mut kept = serde_json::Map::new();
                for (key, field) in fields {
                    let field = match (key.as_str(), field) {
                        ("body" | "items", Json::Array(items)) => Json::Array(
                            items
                                .iter()
                                .filter(|item| !is_input(item))
                                .map(|item| strip(item, depth + 1))
                                .filter(|item| !is_empty_action_set(item))
                                .collect(),
                        ),
                        ("actions", Json::Array(actions)) => {
                            Json::Array(actions.iter().filter(|action| !is_submit(action)).cloned().collect())
                        }
                        (_, other) => strip(other, depth + 1),
                    };
                    kept.insert(key.clone(), field);
                }
                Json::Object(kept)
            }
            Json::Array(items) if depth <= 8 => Json::Array(items.iter().map(|item| strip(item, depth + 1)).collect()),
            other => other.clone(),
        }
    }
    strip(card, 0)
}

#[cfg_attr(not(feature = "teams"), allow(dead_code))]
fn is_empty_action_set(element: &Json) -> bool {
    element.get("type").and_then(Json::as_str) == Some("ActionSet")
        && element.get("actions").and_then(Json::as_array).is_none_or(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_card_with_a_text_field_or_two_inputs_is_not_answered_by_buttons() {
        let actions = json!({"type": "AdaptiveCard", "body": [{"type": "TextBlock", "text": "Refund?"}],
            "actions": [{"type": "Action.Submit", "title": "Yes"}, {"type": "Action.Submit", "title": "No"}]});
        assert!(buttons_can_answer(&actions, 2));
        let choice = json!({"type": "AdaptiveCard", "body": [{"type": "Container", "items": [
            {"type": "Input.ChoiceSet", "id": "size", "choices": [{"title": "S", "value": "s"}, {"title": "M", "value": "m"}]}]}]});
        assert!(buttons_can_answer(&choice, 2));
        assert!(!buttons_can_answer(&choice, 1), "the choices must be the options");
        let text = json!({"type": "AdaptiveCard", "body": [{"type": "Input.Text", "id": "note"}]});
        assert!(!buttons_can_answer(&text, 1));
        let multi = json!({"type": "AdaptiveCard", "body": [
            {"type": "Input.ChoiceSet", "id": "a", "isMultiSelect": true, "choices": [{"title": "A", "value": "a"}]}]});
        assert!(!buttons_can_answer(&multi, 1));
    }

    #[test]
    fn inputs_and_submit_actions_leave_and_links_stay() {
        let card = json!({"type": "AdaptiveCard", "body": [
            {"type": "TextBlock", "text": "Pick"},
            {"type": "Input.ChoiceSet", "id": "size", "choices": []},
            {"type": "ActionSet", "actions": [{"type": "Action.Submit", "title": "Go"}]},
            {"type": "ActionSet", "actions": [{"type": "Action.OpenUrl", "title": "Docs", "url": "https://e.com"}]}],
            "actions": [{"type": "Action.Submit", "title": "Send"}, {"type": "Action.OpenUrl", "title": "Help", "url": "https://h.com"}]});
        let stripped = without_answers(&card);
        assert_eq!(stripped["body"].as_array().map(Vec::len), Some(2), "{stripped}");
        assert_eq!(stripped["body"][1]["actions"][0]["title"], "Docs");
        assert_eq!(stripped["actions"], json!([{"type": "Action.OpenUrl", "title": "Help", "url": "https://h.com"}]));
    }
}
