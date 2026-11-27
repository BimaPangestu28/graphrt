//! Answers and questions for channels that draw cards (Slack, Teams): the same
//! numbered choices as [`super::reply`], with each Adaptive Card kept as a card
//! next to its text fallback, so the channel shows it natively where it can.
//!
//! The numbers stay those of the text version: a tap sends a choice's number,
//! and [`super::reply::pick`] reads it like a typed reply.

use graphrt_channel::{Button, RichMessage, RichPart};
use graphrt_core::Value;
use graphrt_ui::fallback::to_text;
use indexmap::IndexMap;
use serde_json::Value as Json;

use super::config::Messages;
use super::reply::{Choice, Question, text_options};
use crate::approvals::Request;

/// Buttons for `choices`: a tap sends the choice's number.
fn buttons(choices: &[Choice]) -> Vec<Button> {
    choices.iter().map(|choice| Button { id: choice.number.to_string(), title: choice.title.clone() }).collect()
}

/// The card inside a validated `adaptive_card` block.
fn card_of(block: &graphrt_ui::UiBlock) -> Option<Json> {
    (block.kind() == graphrt_ui::UiKind::AdaptiveCard).then(|| block.as_json().get("card").cloned()).flatten()
}

/// The answer of a finished run as a rich message, with the numbered choices
/// of its cards (the same as [`super::reply::answer_of`] numbers them).
pub fn answer_message(
    messages: &Messages,
    answer: &str,
    outputs: &IndexMap<String, Value>,
) -> (RichMessage, Vec<Choice>) {
    let mut message = RichMessage::default();
    let answer = answer.trim();
    if !answer.is_empty() {
        message.parts.push(RichPart::Markdown(answer.to_owned()));
    }
    let mut choices: Vec<Choice> = Vec::new();
    for output in outputs.values() {
        for block in graphrt_core::ui::collect_ui_blocks(output) {
            // A block that does not validate is not shown: its text could be anything.
            let Ok(block) = graphrt_ui::validate(&block.to_json()) else { continue };
            let fallback = to_text(&block, &text_options(messages, choices.len() + 1));
            let card_choices: Vec<Choice> = fallback
                .choices
                .into_iter()
                .map(|choice| Choice { number: choice.number, title: choice.title, input: None })
                .collect();
            match card_of(&block) {
                Some(card) => message.parts.push(RichPart::Card {
                    card,
                    fallback: fallback.text,
                    choices: buttons(&card_choices),
                }),
                None if !fallback.text.is_empty() => message.parts.push(RichPart::Markdown(fallback.text)),
                None => {}
            }
            choices.extend(card_choices);
        }
    }
    (message, choices)
}

/// The question of `request` as a rich message under `heading`: its card when
/// it has one, else its text with the choices as buttons.
pub fn question_message(heading: Option<&str>, request: &Request, question: &Question) -> RichMessage {
    let mut message = RichMessage::default();
    if let Some(heading) = heading.filter(|heading| !heading.trim().is_empty()) {
        message.parts.push(RichPart::Markdown(heading.to_owned()));
    }
    let card = (request.kind == "human_input")
        .then(|| request.data.to_json())
        .and_then(|data| data.get("card").cloned())
        .and_then(|card| graphrt_ui::validate(&card).ok())
        .and_then(|block| card_of(&block));
    match card {
        Some(card) => message.parts.push(RichPart::Card {
            card,
            fallback: question.text.clone(),
            choices: buttons(&question.choices),
        }),
        None => {
            message.parts.push(RichPart::Markdown(question.text.clone()));
            message.buttons = buttons(&question.choices);
        }
    }
    message
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    fn card_block(title: &str) -> Value {
        Value::from_json(json!({
            "$ui": "adaptive_card", "version": 1,
            "card": {"type": "AdaptiveCard", "version": "1.5",
                     "body": [{"type": "TextBlock", "text": title}],
                     "actions": [{"type": "Action.Submit", "id": "yes", "title": "Yes"},
                                 {"type": "Action.Submit", "id": "no", "title": "No"}]},
        }))
    }

    #[test]
    fn cards_stay_cards_and_their_choices_are_numbered_across_the_answer() {
        let mut outputs = IndexMap::new();
        outputs.insert("a".to_owned(), card_block("First?"));
        outputs.insert("b".to_owned(), card_block("Second?"));
        let table =
            json!({"$ui": "table", "version": 1, "columns": [{"key": "k", "label": "K"}], "rows": [{"k": "v"}]});
        outputs.insert("c".to_owned(), Value::from_json(table));
        let (message, choices) = answer_message(&Messages::default(), "Here you go", &outputs);
        assert_eq!(message.parts.len(), 4);
        assert_eq!(message.parts[0], RichPart::Markdown("Here you go".to_owned()));
        let RichPart::Card { choices: second, fallback, .. } = &message.parts[2] else { panic!("a card") };
        assert_eq!(second.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), ["3", "4"]);
        assert!(fallback.contains("3. Yes"), "{fallback}");
        assert!(matches!(&message.parts[3], RichPart::Markdown(text) if text.contains('v')));
        assert_eq!(choices.iter().map(|c| c.number).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // The same numbers as the text version, so a tap and a typed number mean the same.
        let text = super::super::reply::answer_of(&Messages::default(), "Here you go", &outputs);
        assert_eq!(text.choices, choices);
    }
}
