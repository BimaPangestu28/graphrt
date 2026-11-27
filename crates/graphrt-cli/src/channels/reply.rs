//! What goes back to a channel, as text: the answer of a run with its UI blocks,
//! and the question of a paused run with the ways to answer it by typing.
//!
//! A channel shows text and a few buttons. So a question is asked in a form a
//! customer can always answer with one message:
//!
//! | The run waits for | The customer sees | And answers with |
//! | --- | --- | --- |
//! | a `human-input` card, or several actions | the text and numbered choices | a number, or the choice's words |
//! | one field with options | the options, numbered | a number |
//! | one field to fill in | the prompt | the value |
//! | a confirmation only | the prompt | any message |
//! | a tool approval | the tool calls, `Approve` / `Reject` | a number |
//!
//! A form with several fields to fill in has no such form: it is
//! [`Question::answerable`] `false` and stays for the approvals inbox.

use graphrt_channel::Button;
use graphrt_core::Value;
use graphrt_ui::fallback::{Markup, TextOptions, to_text};
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

use super::config::Messages;
use crate::approvals::Request;

/// The action a `human-input` node takes when it names none.
const DEFAULT_ACTION: &str = "submit";
/// Longest text of blocks added to one answer, in characters.
const MAX_BLOCK_CHARS: usize = 3000;
/// Most choices offered as buttons. More are numbered in the text only.
const MAX_BUTTONS: usize = 8;

/// How blocks are written for a channel. Both get Markdown, which the channel
/// crate converts together with the rest of the message.
pub(super) fn text_options(messages: &Messages, first_choice: usize) -> TextOptions {
    let mut options = TextOptions::markdown().with_choice_prompt(messages.choice_prompt.clone());
    options.markup = Markup::Markdown;
    options.max_chars = MAX_BLOCK_CHARS;
    options.with_first_choice(first_choice)
}

/// One way to answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// As written in the text, from 1.
    pub number: usize,
    pub title: String,
    /// What the run resumes with, or `None` for a choice of a finished answer.
    pub input: Option<Json>,
}

/// Buttons for `choices`: a tap sends the choice's number.
pub fn buttons_of(choices: &[Choice]) -> Vec<Button> {
    if choices.len() > MAX_BUTTONS {
        return Vec::new();
    }
    choices.iter().map(|choice| Button { id: choice.number.to_string(), title: choice.title.clone() }).collect()
}

/// The choice a reply picks: its number (`2`, `2.`, `#2`) or its title.
pub fn pick<'a>(choices: &'a [Choice], reply: &str) -> Option<&'a Choice> {
    let reply = reply.trim();
    let digits = reply.trim_start_matches('#').trim_end_matches(['.', ')']).trim();
    if !digits.is_empty() && digits.len() <= 3 && digits.bytes().all(|b| b.is_ascii_digit()) {
        let number: usize = digits.parse().ok()?;
        return choices.iter().find(|choice| choice.number == number);
    }
    let wanted = reply.to_lowercase();
    choices.iter().find(|choice| choice.title.trim().to_lowercase() == wanted)
}

/// The answer of a finished run, ready to send.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    /// Markdown. Empty when the run said nothing.
    pub text: String,
    /// The numbered choices of cards in the answer: the customer's next message
    /// may pick one, and is then read as the choice's words.
    pub choices: Vec<Choice>,
}

/// The answer text of a run and the UI blocks of its outputs, as one message.
pub fn answer_of(messages: &Messages, answer: &str, outputs: &IndexMap<String, Value>) -> Answer {
    let mut parts: Vec<String> = Vec::new();
    let answer = answer.trim();
    if !answer.is_empty() {
        parts.push(answer.to_owned());
    }
    let mut choices = Vec::new();
    for output in outputs.values() {
        for block in graphrt_core::ui::collect_ui_blocks(output) {
            // A block that does not validate is not shown: its text could be anything.
            let Ok(block) = graphrt_ui::validate(&block.to_json()) else { continue };
            let fallback = to_text(&block, &text_options(messages, choices.len() + 1));
            if !fallback.text.is_empty() {
                parts.push(fallback.text);
            }
            choices.extend(fallback.choices.into_iter().map(|choice| Choice {
                number: choice.number,
                title: choice.title,
                input: None,
            }));
        }
    }
    Answer { text: parts.join("\n\n"), choices }
}

/// How one field is filled in from a typed reply.
#[derive(Clone, Debug, PartialEq)]
pub struct FreeText {
    field: String,
    action: String,
    number: bool,
}

/// The question of one request of a paused run.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// Markdown, with the numbered choices written out.
    pub text: String,
    pub choices: Vec<Choice>,
    /// The reply is the value of one field.
    free_text: Option<FreeText>,
    /// Any reply continues the run, with this input.
    proceed: Option<Json>,
    /// Whether one message can answer it.
    pub answerable: bool,
}

impl Question {
    /// The resume input a reply gives, or `None` when the reply does not answer.
    /// `choice` is the id of a tapped button.
    pub fn answer(&self, reply: &str, choice: Option<&str>) -> Option<Json> {
        if !self.choices.is_empty() {
            return pick(&self.choices, choice.unwrap_or(reply)).and_then(|choice| choice.input.clone());
        }
        if let Some(free) = &self.free_text {
            let reply = reply.trim();
            let value = if free.number {
                // `12.5`, and `12,5` as much of the world writes it.
                Json::from(reply.replace(',', ".").parse::<f64>().ok()?)
            } else if reply.is_empty() {
                return None;
            } else {
                Json::from(reply)
            };
            return Some(json!({ "action": free.action, "values": { free.field.clone(): value } }));
        }
        self.proceed.clone()
    }
}

fn text_field<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).map(str::trim).filter(|text| !text.is_empty())
}

fn list<'a>(value: &'a Json, key: &str) -> &'a [Json] {
    value.get(key).and_then(Json::as_array).map_or(&[], Vec::as_slice)
}

/// `text` with numbered `choices` under it.
fn with_choices(text: &str, choices: &[Choice], messages: &Messages) -> String {
    let lines: Vec<String> = choices.iter().map(|choice| format!("{}. {}", choice.number, choice.title)).collect();
    let numbered = format!("{}\n{}", messages.choice_prompt, lines.join("\n"));
    if text.is_empty() { numbered } else { format!("{text}\n\n{numbered}") }
}

fn numbered(titles_and_inputs: Vec<(String, Json)>) -> Vec<Choice> {
    titles_and_inputs
        .into_iter()
        .enumerate()
        .map(|(index, (title, input))| Choice { number: index + 1, title, input: Some(input) })
        .collect()
}

/// The question of a request that waits for a person.
pub fn question_of(request: &Request, messages: &Messages) -> Question {
    let data = request.data.to_json();
    match request.kind.as_str() {
        "human_input" => human_input_question(&data, messages),
        "tool_approval" => tool_approval_question(&data, messages),
        _ => Question { text: String::new(), choices: Vec::new(), free_text: None, proceed: None, answerable: false },
    }
}

fn tool_approval_question(data: &Json, messages: &Messages) -> Question {
    let calls: Vec<String> = list(data, "calls")
        .iter()
        .map(|call| {
            let name = text_field(call, "name").unwrap_or("a tool");
            let arguments = call.get("arguments").map(Json::to_string).unwrap_or_default();
            let arguments: String = arguments.chars().take(300).collect();
            format!("- `{name}` {arguments}")
        })
        .collect();
    let text = format!("The agent asks to run:\n{}", calls.join("\n"));
    let choices = numbered(vec![
        ("Approve".to_owned(), json!({ "approved": true })),
        ("Reject".to_owned(), json!({ "approved": false })),
    ]);
    Question {
        text: with_choices(&text, &choices, messages),
        choices,
        free_text: None,
        proceed: None,
        answerable: true,
    }
}

fn human_input_question(data: &Json, messages: &Messages) -> Question {
    let actions: Vec<(String, String)> = list(data, "actions")
        .iter()
        .filter_map(|action| {
            let id = text_field(action, "id")?;
            Some((id.to_owned(), text_field(action, "title").unwrap_or(id).to_owned()))
        })
        .collect();
    let default_action = actions.first().map_or_else(|| DEFAULT_ACTION.to_owned(), |(id, _)| id.clone());
    if let Some(card) = data.get("card").filter(|card| card.is_object())
        && let Ok(block) = graphrt_ui::validate(card)
    {
        let fallback = to_text(&block, &text_options(messages, 1));
        let choices: Vec<Choice> = fallback
            .choices
            .into_iter()
            .map(|choice| Choice {
                number: choice.number,
                title: choice.title,
                input: Some(json!({
                    "action": choice.action.unwrap_or_else(|| default_action.clone()),
                    "values": choice.values,
                })),
            })
            .collect();
        let proceed = choices.is_empty().then(|| json!({ "action": default_action, "values": {} }));
        return Question { text: fallback.text, choices, free_text: None, proceed, answerable: true };
    }
    let prompt = text_field(data, "prompt").unwrap_or_default().to_owned();
    let fields = list(data, "fields");
    let name_of = |field: &Json| text_field(field, "name").map(str::to_owned);
    let default_of = |field: &Json| field.get("default").filter(|default| !default.is_null()).cloned();
    // What a person who types nothing sends: every field's default.
    let defaults: serde_json::Map<String, Json> =
        fields.iter().filter_map(|field| Some((name_of(field)?, default_of(field)?))).collect();
    let needs_typing =
        |field: &Json| field.get("required").and_then(Json::as_bool) == Some(true) && default_of(field).is_none();
    let mut question =
        Question { text: prompt.clone(), choices: Vec::new(), free_text: None, proceed: None, answerable: true };
    match (fields, actions.len()) {
        ([], 0 | 1) => question.proceed = Some(json!({ "action": default_action, "values": {} })),
        ([field], 0 | 1) => {
            let Some(name) = name_of(field) else {
                question.answerable = false;
                return question;
            };
            let label = text_field(field, "label").unwrap_or(&name).to_owned();
            let options = list(field, "options");
            if options.is_empty() {
                let number = text_field(field, "type") == Some("number");
                question.free_text = Some(FreeText { field: name, action: default_action, number });
                if prompt.is_empty() {
                    question.text = label;
                }
            } else {
                question.choices = numbered(
                    options
                        .iter()
                        .map(|option| {
                            let value = option.get("value").cloned().unwrap_or_else(|| option.clone());
                            let title = text_field(option, "label")
                                .map(str::to_owned)
                                .or_else(|| value.as_str().map(str::to_owned))
                                .unwrap_or_else(|| value.to_string());
                            (title, json!({ "action": default_action, "values": { name.clone(): value } }))
                        })
                        .collect(),
                );
                let text = if prompt.is_empty() { label } else { prompt };
                question.text = with_choices(&text, &question.choices, messages);
            }
        }
        (fields, _) if !actions.is_empty() && !fields.iter().any(needs_typing) => {
            question.choices = numbered(
                actions
                    .iter()
                    .map(|(id, title)| (title.clone(), json!({ "action": id, "values": defaults.clone() })))
                    .collect(),
            );
            question.text = with_choices(&prompt, &question.choices, messages);
        }
        _ => question.answerable = false,
    }
    question
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn request(kind: &str, data: Json) -> Request {
        Request {
            key: "ask".into(),
            node_id: "ask".into(),
            kind: kind.into(),
            data: Value::from_json(data),
            expires_ms: None,
        }
    }

    fn messages() -> Messages {
        Messages::default()
    }

    #[test]
    fn an_answer_carries_its_blocks_as_text() {
        let outputs = IndexMap::from([(
            "data".to_owned(),
            Value::from_json(json!({"$ui": "stats", "version": 1,
                "items": [{"label": "Orders", "value": 42}]})),
        )]);
        let answer = answer_of(&messages(), "  Here is today.  ", &outputs);
        assert!(answer.text.starts_with("Here is today.\n\n"), "{}", answer.text);
        assert!(answer.text.contains("Orders: 42"), "{}", answer.text);
        assert!(answer.choices.is_empty());
        assert_eq!(answer_of(&messages(), " ", &IndexMap::new()), Answer::default());
        // What is not a valid block is left out.
        let invalid = IndexMap::from([("x".to_owned(), Value::from_json(json!({"$ui": "chart", "version": 99})))]);
        assert_eq!(answer_of(&messages(), "ok", &invalid).text, "ok");
    }

    #[test]
    fn a_card_in_an_answer_numbers_its_buttons() {
        let card = json!({"$ui": "adaptive_card", "version": 1, "card": {"type": "AdaptiveCard", "version": "1.5",
            "body": [{"type": "TextBlock", "text": "Which plan?"}],
            "actions": [{"type": "Action.Submit", "id": "basic", "title": "Basic"},
                        {"type": "Action.Submit", "id": "pro", "title": "Pro"}]}});
        let answer = answer_of(&messages(), "", &IndexMap::from([("card".to_owned(), Value::from_json(card))]));
        assert!(answer.text.ends_with("Reply with a number:\n1. Basic\n2. Pro"), "{}", answer.text);
        assert_eq!(pick(&answer.choices, " 2. ").unwrap().title, "Pro");
        assert_eq!(pick(&answer.choices, "basic").unwrap().number, 1);
        assert!(pick(&answer.choices, "3").is_none());
        assert_eq!(buttons_of(&answer.choices)[1], Button { id: "2".into(), title: "Pro".into() });
    }

    #[test]
    fn several_actions_become_numbered_choices_with_the_defaults() {
        let data = json!({"prompt": "Refund Rp 250.000?",
            "fields": [{"name": "note", "default": "ok"}],
            "actions": [{"id": "approve", "title": "Approve"}, {"id": "reject", "title": "Reject"}]});
        let question = question_of(&request("human_input", data), &messages());
        assert!(question.answerable);
        assert_eq!(question.text, "Refund Rp 250.000?\n\nReply with a number:\n1. Approve\n2. Reject");
        assert_eq!(question.answer("2", None).unwrap(), json!({"action": "reject", "values": {"note": "ok"}}));
        assert_eq!(question.answer("whatever", Some("1")).unwrap()["action"], "approve");
        assert_eq!(question.answer("approve", None).unwrap()["action"], "approve");
        assert_eq!(question.answer("maybe", None), None);
    }

    #[test]
    fn one_field_takes_the_reply_as_its_value() {
        let text = json!({"prompt": "What is your order number?", "fields": [{"name": "order", "required": true}]});
        let question = question_of(&request("human_input", text), &messages());
        assert_eq!(question.text, "What is your order number?");
        assert_eq!(
            question.answer(" A-1042 ", None).unwrap(),
            json!({"action": "submit", "values": {"order": "A-1042"}})
        );
        assert_eq!(question.answer("  ", None), None);
        let number = json!({"fields": [{"name": "amount", "label": "Amount", "type": "number"}],
            "actions": [{"id": "send"}]});
        let question = question_of(&request("human_input", number), &messages());
        assert_eq!(question.text, "Amount");
        assert_eq!(question.answer("12,5", None).unwrap(), json!({"action": "send", "values": {"amount": 12.5}}));
        assert_eq!(question.answer("twelve", None), None);
    }

    #[test]
    fn one_field_with_options_numbers_them() {
        let data = json!({"prompt": "Size?", "fields": [{"name": "size",
            "options": [{"value": "s", "label": "Small"}, "large"]}]});
        let question = question_of(&request("human_input", data), &messages());
        assert_eq!(question.text, "Size?\n\nReply with a number:\n1. Small\n2. large");
        assert_eq!(question.answer("1", None).unwrap(), json!({"action": "submit", "values": {"size": "s"}}));
        assert_eq!(question.answer("large", None).unwrap()["values"]["size"], "large");
    }

    #[test]
    fn a_confirmation_continues_on_any_reply_and_a_long_form_is_not_for_a_chat() {
        let confirm = question_of(&request("human_input", json!({"prompt": "Ready?"})), &messages());
        assert_eq!(confirm.answer("ok", None).unwrap(), json!({"action": "submit", "values": {}}));
        let form = json!({"prompt": "Details", "fields": [
            {"name": "name", "required": true}, {"name": "address", "required": true}],
            "actions": [{"id": "send"}]});
        assert!(!question_of(&request("human_input", form), &messages()).answerable);
        assert!(!question_of(&request("something_else", json!({})), &messages()).answerable);
    }

    #[test]
    fn a_card_question_answers_with_the_cards_action() {
        let data = json!({"actions": [{"id": "approve"}, {"id": "reject"}],
            "card": {"$ui": "adaptive_card", "version": 1, "card": {"type": "AdaptiveCard", "version": "1.5",
                "body": [{"type": "TextBlock", "text": "Refund to Sari?"}],
                "actions": [{"type": "Action.Submit", "id": "approve", "title": "Yes"},
                            {"type": "Action.Submit", "id": "reject", "title": "No"}]}}});
        let question = question_of(&request("human_input", data), &messages());
        assert!(question.text.starts_with("Refund to Sari?"), "{}", question.text);
        assert_eq!(question.answer("2", None).unwrap()["action"], "reject");
    }

    #[test]
    fn a_tool_approval_shows_the_calls_and_takes_approve_or_reject() {
        let data = json!({"calls": [{"id": "c1", "name": "refund", "arguments": {"amount": 250000}}]});
        let question = question_of(&request("tool_approval", data), &messages());
        assert!(question.text.contains("`refund` {\"amount\":250000}"), "{}", question.text);
        assert_eq!(question.answer("1", None).unwrap(), json!({"approved": true}));
        assert_eq!(question.answer("Reject", None).unwrap(), json!({"approved": false}));
    }
}
