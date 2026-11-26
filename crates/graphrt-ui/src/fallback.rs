//! Blocks as text, for channels that cannot draw them (WhatsApp, SMS, e-mail, a terminal).
//!
//! [`to_text`] turns a validated block into tidy text:
//!
//! | kind            | becomes                                                              |
//! |-----------------|----------------------------------------------------------------------|
//! | `adaptive_card` | its text, facts and input values, links as `Title: url`, and the submit buttons as numbered [`Choice`]s |
//! | `table`         | one compact line per row, the first [`TextOptions::max_rows`] rows   |
//! | `stats`         | one line per tile: `Revenue: Rp 125.000.000 (▲ 12.5% vs last week)`  |
//! | `chart`         | the values when they are few, otherwise each series' range and total |
//! | `diagram`       | its title and `fallback_text` (a picture has no fair text form)      |
//!
//! A block's own `fallback_text` (or a card's `fallbackText`) replaces the generated body:
//! the author said what to tell people there. A card keeps its numbered choices either way.
//!
//! ```
//! use graphrt_ui::fallback::{TextOptions, match_choice, to_text};
//! use serde_json::json;
//!
//! let card = graphrt_ui::validate(&json!({
//!     "$ui": "adaptive_card", "version": 1,
//!     "card": {"type": "AdaptiveCard", "version": "1.5",
//!              "body": [{"type": "TextBlock", "text": "Refund **Rp 250.000** to Sari?"}],
//!              "actions": [{"type": "Action.Submit", "id": "approve", "title": "Approve"},
//!                          {"type": "Action.Submit", "id": "reject", "title": "Reject"}]},
//! })).unwrap();
//! let fallback = to_text(&card, &TextOptions::whatsapp());
//! assert_eq!(fallback.text, "Refund *Rp 250.000* to Sari?\n\nReply with a number:\n1. Approve\n2. Reject");
//! // The customer answers "2": the host resumes with the card's action, as a button would.
//! let choice = match_choice(&fallback.choices, " 2. ").unwrap();
//! assert_eq!(choice.action.as_deref(), Some("reject"));
//! ```
//!
//! Numbered choices: every `Action.Submit` / `Action.Execute` of the card, in order. A card
//! whose only question is one single-choice `Input.ChoiceSet` (with at most one submit
//! button) numbers the options of that input instead. Each [`Choice`] carries what the
//! button would send: the action's key and the values (the action's `data`, then every
//! input's starting value), the same shape a rendering host answers `human-input` with.

use serde_json::{Map, Value as Json};

use crate::{UiBlock, UiKind};

/// How emphasis is written in the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Markup {
    /// No markers at all (SMS, plain e-mail).
    #[default]
    Plain,
    /// WhatsApp: `*bold*`.
    WhatsApp,
    /// Markdown: `**bold**` (Telegram, Slack, a terminal that renders it).
    Markdown,
}

impl Markup {
    fn bold(self, text: &str) -> String {
        match self {
            Markup::Plain => text.to_owned(),
            Markup::WhatsApp => format!("*{text}*"),
            Markup::Markdown => format!("**{text}**"),
        }
    }
}

/// How [`to_text`] writes a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextOptions {
    pub markup: Markup,
    /// Longest text, in characters; the rest is cut with `…` (WhatsApp takes 4096).
    pub max_chars: usize,
    /// Most table rows written before "... and N more".
    pub max_rows: usize,
    /// A chart with at most this many categories lists every value; a longer one is summarised.
    pub max_points: usize,
    /// Thousands separator (`,` in English, `.` in Indonesian).
    pub thousands: char,
    /// Decimal separator (`.` in English, `,` in Indonesian).
    pub decimal: char,
    /// The line above numbered choices.
    pub choice_prompt: String,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            markup: Markup::Plain,
            max_chars: 3500,
            max_rows: 10,
            max_points: 12,
            thousands: ',',
            decimal: '.',
            choice_prompt: "Reply with a number:".to_owned(),
        }
    }
}

impl TextOptions {
    /// Defaults with WhatsApp emphasis.
    pub fn whatsapp() -> Self {
        Self { markup: Markup::WhatsApp, ..Self::default() }
    }

    /// Defaults with Markdown emphasis.
    pub fn markdown() -> Self {
        Self { markup: Markup::Markdown, ..Self::default() }
    }

    /// Writes numbers with these separators, e.g. `('.', ',')` for Indonesian.
    pub fn with_separators(mut self, thousands: char, decimal: char) -> Self {
        self.thousands = thousands;
        self.decimal = decimal;
        self
    }

    /// Replaces the line above numbered choices (to say it in the customer's language).
    pub fn with_choice_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.choice_prompt = prompt.into();
        self
    }
}

/// One numbered answer of a card.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// 1-based, as written in the text.
    pub number: usize,
    /// The button's (or option's) title.
    pub title: String,
    /// The answer key of the submit action: `Action.Execute`'s `verb`, else the action's
    /// `id`, else `data.action`, else its title. `None` when the card has no submit action.
    pub action: Option<String>,
    /// What the button sends: the action's `data`, then every input's value.
    pub values: Map<String, Json>,
}

/// A block as text.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TextFallback {
    /// Ready to send, numbered choices included.
    pub text: String,
    /// The numbered answers in `text`; empty unless the block is a card with submit actions.
    pub choices: Vec<Choice>,
}

/// Writes `block` as text (see the module docs).
pub fn to_text(block: &UiBlock, options: &TextOptions) -> TextFallback {
    let value = block.as_json();
    let mut lines: Vec<String> = Vec::new();
    if let Some(title) = text_field(value, "title") {
        lines.push(options.markup.bold(title));
    }
    if let Some(subtitle) = text_field(value, "subtitle") {
        lines.push(subtitle.to_owned());
    }
    let author_text = text_field(value, "fallback_text");
    let mut choices = Vec::new();
    match block.kind() {
        UiKind::AdaptiveCard => {
            let card = value.get("card").unwrap_or(&Json::Null);
            let mut reader = CardReader::new(options);
            reader.read_card(card);
            let author_text = author_text.or_else(|| text_field(card, "fallbackText"));
            match author_text {
                Some(text) => lines.push(text.to_owned()),
                None => {
                    // The title often repeats the card's first heading.
                    let repeats_title = lines.first().is_some_and(|title| reader.lines.first() == Some(title));
                    lines.extend(reader.lines.iter().skip(usize::from(repeats_title)).cloned());
                    lines.extend(reader.links.iter().cloned());
                }
            }
            choices = reader.into_choices();
        }
        _ if author_text.is_some() => lines.extend(author_text.map(str::to_owned)),
        UiKind::Table => table_lines(value, options, &mut lines),
        UiKind::Stats => stats_lines(value, options, &mut lines),
        UiKind::Chart => chart_lines(value, options, &mut lines),
        UiKind::Diagram => {
            if lines.is_empty() {
                let kind = text_field(value, "source").and_then(crate::diagram::diagram_type).unwrap_or("diagram");
                lines.push(format!("[{}]", diagram_name(kind)));
            }
        }
    }
    let mut text = lines.join("\n").trim().to_owned();
    if !choices.is_empty() {
        let list: Vec<String> = choices.iter().map(|choice| format!("{}. {}", choice.number, choice.title)).collect();
        let list = format!("{}\n{}", options.choice_prompt, list.join("\n"));
        // Cut the body, never the choices: they are how the customer answers.
        let room = options.max_chars.saturating_sub(list.chars().count() + 2);
        text = truncate(&text, room);
        text = if text.is_empty() { list } else { format!("{text}\n\n{list}") };
    } else {
        text = truncate(&text, options.max_chars);
    }
    TextFallback { text, choices }
}

/// The choice a customer's reply picks: its number (`2`, `2.`, `2)`, `#2`) or its title,
/// ignoring case and surrounding spaces. `None` when the reply is something else, which
/// the host then treats as an ordinary message.
pub fn match_choice<'a>(choices: &'a [Choice], reply: &str) -> Option<&'a Choice> {
    let reply = reply.trim();
    let digits = reply.trim_start_matches('#').trim_end_matches(['.', ')']).trim();
    if !digits.is_empty() && digits.len() <= 3 && digits.bytes().all(|b| b.is_ascii_digit()) {
        let number: usize = digits.parse().ok()?;
        return choices.iter().find(|choice| choice.number == number);
    }
    let wanted = reply.to_lowercase();
    choices.iter().find(|choice| choice.title.trim().to_lowercase() == wanted)
}

fn text_field<'a>(value: &'a Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Json::as_str).map(str::trim).filter(|text| !text.is_empty())
}

fn items<'a>(value: &'a Json, key: &str) -> &'a [Json] {
    value.get(key).and_then(Json::as_array).map_or(&[], Vec::as_slice)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

fn diagram_name(kind: &str) -> &'static str {
    match kind {
        "flowchart" | "graph" => "Flowchart",
        "sequenceDiagram" => "Sequence diagram",
        "classDiagram" => "Class diagram",
        "stateDiagram" | "stateDiagram-v2" => "State diagram",
        "erDiagram" => "Entity diagram",
        "gantt" => "Gantt chart",
        "pie" => "Pie chart",
        "mindmap" => "Mind map",
        "timeline" => "Timeline",
        "quadrantChart" => "Quadrant chart",
        "gitGraph" => "Git graph",
        _ => "Diagram",
    }
}

// --- Numbers ---------------------------------------------------------------------------------

/// Digits of `value` rounded to `decimals` places, grouped by thousands, without trailing zeros.
fn grouped(value: f64, decimals: usize, options: &TextOptions) -> String {
    let fixed = format!("{:.*}", decimals, value.abs());
    let (whole, fraction) = fixed.split_once('.').unwrap_or((fixed.as_str(), ""));
    let fraction = fraction.trim_end_matches('0');
    let mut out = String::new();
    let is_zero = whole.bytes().all(|b| b == b'0') && fraction.is_empty();
    if value < 0.0 && !is_zero {
        out.push('-');
    }
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(options.thousands);
        }
        out.push(digit);
    }
    if !fraction.is_empty() {
        out.push(options.decimal);
        out.push_str(fraction);
    }
    out
}

pub(crate) fn currency_prefix(code: &str) -> (String, usize) {
    match code {
        "IDR" => ("Rp ".to_owned(), 0),
        "USD" => ("$".to_owned(), 2),
        "EUR" => ("€".to_owned(), 2),
        "GBP" => ("£".to_owned(), 2),
        "JPY" => ("¥".to_owned(), 0),
        "SGD" => ("S$".to_owned(), 2),
        "MYR" => ("RM ".to_owned(), 2),
        other => (format!("{other} "), 2),
    }
}

/// A number in one of [`crate::chart::NUMBER_FORMATS`].
pub(crate) fn format_number(value: f64, format: Option<&str>, currency: Option<&str>, options: &TextOptions) -> String {
    if !value.is_finite() {
        return "-".to_owned();
    }
    match format.map(str::to_ascii_lowercase).as_deref() {
        Some("percent") => format!("{}%", grouped(value, 1, options)),
        Some("compact") => compact(value, options),
        Some("currency") => {
            let (prefix, decimals) = currency.map(currency_prefix).unwrap_or_default();
            let digits = grouped(value.abs(), decimals, options);
            format!("{}{prefix}{digits}", if value < 0.0 { "-" } else { "" })
        }
        _ => grouped(value, 2, options),
    }
}

pub(crate) fn compact(value: f64, options: &TextOptions) -> String {
    let magnitude = value.abs();
    let (scaled, suffix) = if magnitude >= 1e9 {
        (value / 1e9, "B")
    } else if magnitude >= 1e6 {
        (value / 1e6, "M")
    } else if magnitude >= 1e3 {
        (value / 1e3, "K")
    } else {
        (value, "")
    };
    format!("{}{suffix}", grouped(scaled, 1, options))
}

/// A table cell or tile value: numbers in `format`, booleans as Yes / No, text as is.
fn format_cell(value: &Json, format: Option<&str>, currency: Option<&str>, options: &TextOptions) -> String {
    match value {
        Json::Null => "-".to_owned(),
        Json::Bool(flag) => if *flag { "Yes" } else { "No" }.to_owned(),
        Json::Number(number) => match (number.as_f64(), format) {
            (Some(number), Some("text" | "date" | "status")) => grouped(number, 2, options),
            (Some(number), _) => format_number(number, format, currency, options),
            (None, _) => number.to_string(),
        },
        Json::String(text) => single_line(text),
        other => other.to_string(),
    }
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- Table, stats, chart ----------------------------------------------------------------------

fn table_lines(table: &Json, options: &TextOptions, lines: &mut Vec<String>) {
    let columns = items(table, "columns");
    let rows = items(table, "rows");
    let labels: Vec<&str> = columns.iter().filter_map(|column| text_field(column, "label")).collect();
    if columns.len() > 1 {
        lines.push(labels.join(" · "));
    }
    for (i, row) in rows.iter().take(options.max_rows).enumerate() {
        let cells: Vec<String> = columns
            .iter()
            .map(|column| {
                let key = column.get("key").and_then(Json::as_str).unwrap_or("");
                let format = column.get("format").and_then(Json::as_str);
                let currency = column.get("currency").and_then(Json::as_str);
                format_cell(row.get(key).unwrap_or(&Json::Null), format, currency, options)
            })
            .collect();
        lines.push(format!("{}. {}", i + 1, cells.join(" · ")));
    }
    if rows.is_empty() {
        lines.push("(no rows)".to_owned());
    } else if rows.len() > options.max_rows {
        lines.push(format!("... and {} more", rows.len() - options.max_rows));
    }
}

fn stats_lines(stats: &Json, options: &TextOptions, lines: &mut Vec<String>) {
    for item in items(stats, "items") {
        let label = text_field(item, "label").unwrap_or("");
        let format = item.get("format").and_then(Json::as_str);
        let currency = item.get("currency").and_then(Json::as_str);
        let value = format_cell(item.get("value").unwrap_or(&Json::Null), format, currency, options);
        let mut notes: Vec<String> = Vec::new();
        if let Some(delta) = item.get("delta").and_then(Json::as_f64) {
            let trend = item.get("trend").and_then(Json::as_str).map(str::to_ascii_lowercase);
            let arrow = match trend.as_deref() {
                Some("up") => "▲",
                Some("down") => "▼",
                Some(_) => "■",
                None if delta > 0.0 => "▲",
                None if delta < 0.0 => "▼",
                None => "■",
            };
            let delta_format = item.get("delta_format").and_then(Json::as_str).filter(|f| *f == "percent");
            notes.push(format!("{arrow} {}", format_number(delta.abs(), delta_format, None, options)));
        }
        if let Some(caption) = text_field(item, "caption") {
            notes.push(caption.to_owned());
        }
        let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(" ")) };
        lines.push(format!("{label}: {value}{notes}"));
    }
}

fn chart_lines(chart: &Json, options: &TextOptions, lines: &mut Vec<String>) {
    let y = chart.get("y").unwrap_or(&Json::Null);
    let format = y.get("format").and_then(Json::as_str);
    let currency = y.get("currency").and_then(Json::as_str);
    let number = |value: f64| format_number(value, format, currency, options);
    let categories: Vec<String> = items(chart.get("x").unwrap_or(&Json::Null), "values")
        .iter()
        .map(|value| value.as_str().map_or_else(|| value.to_string(), single_line))
        .collect();
    let series: Vec<(&str, Vec<Option<f64>>)> = items(chart, "series")
        .iter()
        .map(|one| {
            let values = items(one, "values").iter().map(Json::as_f64).collect();
            (text_field(one, "name").unwrap_or(""), values)
        })
        .collect();
    let is_round = matches!(chart.get("type").and_then(Json::as_str), Some("pie" | "donut"));
    let cell = |value: Option<f64>| value.map_or_else(|| "-".to_owned(), &number);

    if is_round {
        let values = series.first().map(|(_, values)| values.as_slice()).unwrap_or_default();
        let total: f64 = values.iter().flatten().sum();
        for (category, value) in categories.iter().zip(values).take(options.max_rows.max(options.max_points)) {
            let share =
                value.filter(|_| total > 0.0).map(|v| format!(" ({}%)", grouped(v / total * 100.0, 1, options)));
            lines.push(format!("{category}: {}{}", cell(*value), share.unwrap_or_default()));
        }
        lines.push(format!("Total: {}", number(total)));
        return;
    }
    if categories.len() <= options.max_points {
        for (i, category) in categories.iter().enumerate() {
            let at = |values: &Vec<Option<f64>>| cell(values.get(i).copied().flatten());
            let line = match series.as_slice() {
                [(_, values)] => at(values),
                many => {
                    many.iter().map(|(name, values)| format!("{name} {}", at(values))).collect::<Vec<_>>().join(", ")
                }
            };
            lines.push(format!("{category}: {line}"));
        }
        return;
    }
    let (first_category, last_category) =
        (categories.first().map_or("", String::as_str), categories.last().map_or("", String::as_str));
    lines.push(format!("{} points, {first_category} to {last_category}", categories.len()));
    for (name, values) in &series {
        let known: Vec<f64> = values.iter().flatten().copied().collect();
        let (Some(first), Some(last)) = (known.first(), known.last()) else {
            lines.push(format!("{name}: no data"));
            continue;
        };
        let low = known.iter().copied().fold(f64::INFINITY, f64::min);
        let high = known.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        lines.push(format!(
            "{name}: {} to {} (low {}, high {})",
            number(*first),
            number(*last),
            number(low),
            number(high)
        ));
    }
}

// --- Adaptive Card ----------------------------------------------------------------------------

struct CardInput {
    id: String,
    kind: String,
    /// The value a submit sends for it.
    value: Json,
    /// `(title, value)` of a single-choice `Input.ChoiceSet`.
    options: Vec<(String, String)>,
}

struct SubmitAction {
    title: String,
    key: Option<String>,
    data: Map<String, Json>,
    sends_inputs: bool,
}

struct CardReader<'a> {
    options: &'a TextOptions,
    lines: Vec<String>,
    links: Vec<String>,
    inputs: Vec<CardInput>,
    submits: Vec<SubmitAction>,
}

impl<'a> CardReader<'a> {
    fn new(options: &'a TextOptions) -> Self {
        Self { options, lines: Vec::new(), links: Vec::new(), inputs: Vec::new(), submits: Vec::new() }
    }

    fn read_card(&mut self, card: &Json) {
        self.read_elements(items(card, "body"));
        self.read_actions(items(card, "actions"));
    }

    fn read_elements(&mut self, elements: &[Json]) {
        for element in elements {
            self.read_element(element);
        }
    }

    fn read_element(&mut self, element: &Json) {
        if element.get("isVisible") == Some(&Json::Bool(false)) {
            return;
        }
        let kind = element.get("type").and_then(Json::as_str).unwrap_or("Column");
        match kind {
            "TextBlock" => {
                let Some(text) = text_field(element, "text") else { return };
                let text = card_markdown(text, self.options.markup);
                let is_heading = element.get("style").and_then(Json::as_str) == Some("heading")
                    || element.get("weight").and_then(Json::as_str).is_some_and(|w| w.eq_ignore_ascii_case("bolder"));
                let has_markers = text.contains('*');
                self.lines.push(if is_heading && !has_markers { self.options.markup.bold(&text) } else { text });
            }
            "RichTextBlock" => {
                let text: String = items(element, "inlines")
                    .iter()
                    .filter_map(|inline| inline.as_str().or_else(|| inline.get("text").and_then(Json::as_str)))
                    .collect();
                if !text.trim().is_empty() {
                    self.lines.push(text.trim().to_owned());
                }
            }
            "Image" => {
                if let Some(alt) = text_field(element, "altText") {
                    self.lines.push(format!("[{alt}]"));
                }
            }
            "ImageSet" => self.read_elements(items(element, "images")),
            "Container" | "Column" => self.read_elements(items(element, "items")),
            "ColumnSet" => self.read_elements(items(element, "columns")),
            "FactSet" => {
                for fact in items(element, "facts") {
                    let title = text_field(fact, "title").unwrap_or("");
                    let value = card_markdown(text_field(fact, "value").unwrap_or("-"), self.options.markup);
                    self.lines.push(format!("{title}: {value}"));
                }
            }
            "Table" => {
                for row in items(element, "rows") {
                    let cells: Vec<String> = items(row, "cells")
                        .iter()
                        .map(|cell| {
                            let mut inner = CardReader::new(self.options);
                            inner.read_elements(items(cell, "items"));
                            inner.lines.join(" ")
                        })
                        .collect();
                    if cells.iter().any(|cell| !cell.is_empty()) {
                        self.lines.push(cells.join(" · "));
                    }
                }
            }
            "ActionSet" => self.read_actions(items(element, "actions")),
            input if input.starts_with("Input.") => self.read_input(element, input),
            _ => {
                if let Some(fallback) = element.get("fallback").filter(|fallback| fallback.is_object()) {
                    self.read_element(fallback);
                }
            }
        }
    }

    fn read_input(&mut self, input: &Json, kind: &str) {
        let Some(id) = text_field(input, "id") else { return };
        let label = text_field(input, "label").or_else(|| text_field(input, "title")).unwrap_or(id);
        let raw = match input.get("value") {
            Some(Json::String(text)) => Some(text.clone()),
            Some(Json::Number(number)) => Some(number.to_string()),
            Some(Json::Bool(flag)) => Some(flag.to_string()),
            _ => None,
        };
        let mut options = Vec::new();
        let (value, shown) = match kind {
            "Input.Number" => {
                let number = raw.as_deref().and_then(|text| text.trim().parse::<f64>().ok()).filter(|n| n.is_finite());
                let value = number.and_then(serde_json::Number::from_f64).map_or(Json::Null, |parsed| {
                    // Keep whole numbers whole: 250000, not 250000.0.
                    input.get("value").filter(|original| original.is_number()).cloned().unwrap_or(Json::Number(parsed))
                });
                (value, number.map(|n| grouped(n, 2, self.options)))
            }
            "Input.Toggle" => {
                let on = text_field(input, "valueOn").unwrap_or("true");
                let off = text_field(input, "valueOff").unwrap_or("false");
                let is_on = raw.as_deref() == Some(on);
                let value = if on == "true" && off == "false" {
                    Json::Bool(is_on)
                } else {
                    Json::String(if is_on { on } else { off }.to_owned())
                };
                (value, Some(if is_on { "Yes" } else { "No" }.to_owned()))
            }
            "Input.ChoiceSet" => {
                let choices: Vec<(String, String)> = items(input, "choices")
                    .iter()
                    .filter_map(|choice| {
                        Some((text_field(choice, "title")?.to_owned(), choice.get("value")?.as_str()?.to_owned()))
                    })
                    .collect();
                let shown = raw.as_ref().and_then(|value| {
                    let titles: Vec<&str> = value
                        .split(',')
                        .filter_map(|one| {
                            choices.iter().find(|(_, v)| v == one.trim()).map(|(title, _)| title.as_str())
                        })
                        .collect();
                    (!titles.is_empty()).then(|| titles.join(", "))
                });
                if input.get("isMultiSelect") != Some(&Json::Bool(true)) {
                    options = choices;
                }
                (Json::String(raw.clone().unwrap_or_default()), shown)
            }
            _ => (
                Json::String(raw.clone().unwrap_or_default()),
                raw.as_deref().map(single_line).filter(|v| !v.is_empty()),
            ),
        };
        if let Some(shown) = shown {
            self.lines.push(format!("{label}: {shown}"));
        } else if !options.is_empty() && input.get("label").is_some() {
            self.lines.push(label.to_owned());
        }
        self.inputs.push(CardInput { id: id.to_owned(), kind: kind.to_owned(), value, options });
    }

    fn read_actions(&mut self, actions: &[Json]) {
        for action in actions {
            let title = text_field(action, "title");
            match action.get("type").and_then(Json::as_str).unwrap_or("") {
                "Action.Submit" | "Action.Execute" => {
                    let data = action.get("data").and_then(Json::as_object).cloned().unwrap_or_default();
                    let verb = text_field(action, "verb")
                        .filter(|_| action.get("type") == Some(&Json::from("Action.Execute")));
                    let key = verb
                        .or_else(|| text_field(action, "id"))
                        .or_else(|| data.get("action").and_then(Json::as_str))
                        .or(title)
                        .map(str::to_owned);
                    let sends_inputs = !action
                        .get("associatedInputs")
                        .and_then(Json::as_str)
                        .is_some_and(|mode| mode.eq_ignore_ascii_case("none"));
                    let title = title.or(key.as_deref()).unwrap_or("OK").to_owned();
                    self.submits.push(SubmitAction { title, key, data, sends_inputs });
                }
                "Action.OpenUrl" => {
                    if let Some(url) = text_field(action, "url") {
                        self.links.push(match title {
                            Some(title) => format!("{title}: {url}"),
                            None => url.to_owned(),
                        });
                    }
                }
                "Action.ShowCard" => {
                    if let Some(card) = action.get("card") {
                        self.read_card(card);
                    }
                }
                _ => {
                    if let Some(fallback) = action.get("fallback").filter(|fallback| fallback.is_object()) {
                        self.read_actions(std::slice::from_ref(fallback));
                    }
                }
            }
        }
    }

    fn input_values(&self) -> Map<String, Json> {
        self.inputs.iter().map(|input| (input.id.clone(), input.value.clone())).collect()
    }

    fn into_choices(self) -> Vec<Choice> {
        let input_values = self.input_values();
        let values_of = |submit: Option<&SubmitAction>| {
            let mut values = submit.map(|submit| submit.data.clone()).unwrap_or_default();
            if submit.is_none_or(|submit| submit.sends_inputs) {
                values.extend(input_values.clone());
            }
            values
        };
        // One single-choice question: its options are the answers.
        let choice_inputs: Vec<&CardInput> =
            self.inputs.iter().filter(|input| input.kind == "Input.ChoiceSet" && !input.options.is_empty()).collect();
        if let ([question], true) = (choice_inputs.as_slice(), self.submits.len() <= 1) {
            let submit = self.submits.first();
            return question
                .options
                .iter()
                .enumerate()
                .map(|(i, (title, value))| {
                    let mut values = values_of(submit);
                    values.insert(question.id.clone(), Json::String(value.clone()));
                    Choice { number: i + 1, title: title.clone(), action: submit.and_then(|s| s.key.clone()), values }
                })
                .collect();
        }
        self.submits
            .iter()
            .enumerate()
            .map(|(i, submit)| Choice {
                number: i + 1,
                title: submit.title.clone(),
                action: submit.key.clone(),
                values: values_of(Some(submit)),
            })
            .collect()
    }
}

/// Card text (the Markdown subset Adaptive Cards allow) in `markup`: `**bold**` follows the
/// channel, `[title](url)` becomes `title (url)` outside Markdown, the text stays one paragraph.
fn card_markdown(text: &str, markup: Markup) -> String {
    let text = text.trim();
    if markup == Markup::Markdown {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let (before, link) = rest.split_at(open);
        let parsed = link.find("](").and_then(|middle| {
            let title = link.get(1..middle)?;
            let after = link.get(middle + 2..)?;
            let close = after.find(')')?;
            (!title.contains('['))
                .then(|| (title, after.get(..close).unwrap_or(""), after.get(close + 1..).unwrap_or("")))
        });
        out.push_str(before);
        match parsed {
            Some((title, url, after)) => {
                out.push_str(&if title == url || title.is_empty() {
                    url.to_owned()
                } else {
                    format!("{title} ({url})")
                });
                rest = after;
            }
            None => {
                out.push('[');
                rest = link.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    match markup {
        Markup::WhatsApp => out.replace("**", "*"),
        _ => out.replace("**", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;
    use serde_json::json;

    fn text(block: Json, options: &TextOptions) -> TextFallback {
        let block = validate(&block).unwrap_or_else(|error| panic!("{error}"));
        to_text(&block, options)
    }

    fn fixture(name: &str) -> Json {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/valid").join(name);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_card_becomes_text_with_numbered_submit_actions() {
        let fallback = text(fixture("adaptive-card-refund-approval.json"), &TextOptions::whatsapp());
        assert_eq!(
            fallback.text,
            "*Refund request*\n\
             Sari Wulandari asks for a refund of *Rp 250.000* for order #1042 (2 items arrived damaged).\n\
             Order: #1042\nPaid: Rp 480.000\nCustomer since: March 2024\n\
             Priority\n*High*\n\
             Reason: Damaged on arrival\nRefund amount (Rp): 250,000\nPut the items back in stock: No\n\
             Open order: https://shop.example.com/orders/1042\n\n\
             Reply with a number:\n1. Approve refund\n2. Reject"
        );
        let titles: Vec<&str> = fallback.choices.iter().map(|choice| choice.title.as_str()).collect();
        assert_eq!(titles, ["Approve refund", "Reject"]);
        let reject = match_choice(&fallback.choices, "2").unwrap();
        assert_eq!(reject.action.as_deref(), Some("reject"));
        // The values a button would send: typed like the rendering host's.
        assert_eq!(
            Json::Object(reject.values.clone()),
            json!({"reason": "damaged", "amount": 250000, "note": "", "restock": false})
        );
    }

    #[test]
    fn plain_and_markdown_markup() {
        let card = json!({"$ui": "adaptive_card", "version": 1, "title": "Order",
        "card": {"type": "AdaptiveCard", "version": "1.5", "body": [
            {"type": "TextBlock", "text": "Pay **today** at [our shop](https://shop.example.com) or [https://pay.example.com](https://pay.example.com)"},
        ]}});
        assert_eq!(
            text(card.clone(), &TextOptions::default()).text,
            "Order\nPay today at our shop (https://shop.example.com) or https://pay.example.com"
        );
        assert_eq!(
            text(card, &TextOptions::markdown()).text,
            "**Order**\nPay **today** at [our shop](https://shop.example.com) or [https://pay.example.com](https://pay.example.com)"
        );
    }

    #[test]
    fn one_choice_question_numbers_its_options() {
        let card = json!({"$ui": "adaptive_card", "version": 1,
            "card": {"type": "AdaptiveCard", "version": "1.5",
                "body": [
                    {"type": "TextBlock", "text": "Which size?"},
                    {"type": "Input.ChoiceSet", "id": "size", "choices": [
                        {"title": "Small", "value": "s"}, {"title": "Large", "value": "l"}]},
                ],
                "actions": [{"type": "Action.Submit", "id": "order", "title": "Order", "data": {"source": "card"}}]}});
        let fallback = text(card, &TextOptions::default().with_choice_prompt("Balas dengan angka:"));
        assert_eq!(fallback.text, "Which size?\n\nBalas dengan angka:\n1. Small\n2. Large");
        let large = match_choice(&fallback.choices, "large").unwrap();
        assert_eq!(large.action.as_deref(), Some("order"));
        assert_eq!(Json::Object(large.values.clone()), json!({"source": "card", "size": "l"}));
    }

    #[test]
    fn action_keys_follow_the_rendering_host() {
        let card = json!({"$ui": "adaptive_card", "version": 1, "fallback_text": "Confirm the order?",
        "card": {"type": "AdaptiveCard", "version": "1.5",
            "body": [{"type": "TextBlock", "text": "not shown"}, {"type": "Input.Text", "id": "note", "value": "hi"}],
            "actions": [
                {"type": "Action.Execute", "verb": "confirm", "id": "ignored", "title": "Confirm"},
                {"type": "Action.Submit", "title": "Later", "data": {"action": "later"}, "associatedInputs": "none"},
                {"type": "Action.Submit", "title": "Cancel"},
                {"type": "Action.ShowCard", "title": "More", "card": {"type": "AdaptiveCard",
                    "actions": [{"type": "Action.Submit", "id": "escalate", "title": "Escalate"}]}},
            ]}});
        let fallback = text(card, &TextOptions::default());
        assert!(
            fallback.text.starts_with(
                "Confirm the order?\n\nReply with a number:\n1. Confirm\n2. Later\n3. Cancel\n4. Escalate"
            )
        );
        let keys: Vec<Option<&str>> = fallback.choices.iter().map(|choice| choice.action.as_deref()).collect();
        assert_eq!(keys, [Some("confirm"), Some("later"), Some("Cancel"), Some("escalate")]);
        assert_eq!(Json::Object(fallback.choices[0].values.clone()), json!({"note": "hi"}));
        assert_eq!(Json::Object(fallback.choices[1].values.clone()), json!({"action": "later"}));
    }

    #[test]
    fn replies_match_a_number_or_a_title() {
        let choices = text(fixture("adaptive-card-refund-approval.json"), &TextOptions::default()).choices;
        for reply in ["1", " 1 ", "1.", "1)", "#1", "approve refund", "APPROVE REFUND"] {
            assert_eq!(match_choice(&choices, reply).map(|choice| choice.number), Some(1), "{reply}");
        }
        for reply in ["3", "0", "yes please", "", "1 box", "12345"] {
            assert!(match_choice(&choices, reply).is_none(), "{reply}");
        }
    }

    #[test]
    fn a_table_becomes_compact_lines() {
        let fallback = text(fixture("table-top-products.json"), &TextOptions::whatsapp().with_separators('.', ','));
        let lines: Vec<&str> = fallback.text.lines().collect();
        assert_eq!(lines[0], "*Produk terlaris*");
        assert!(lines.iter().any(|line| line.starts_with("1. ") && line.contains(" · ")), "{}", fallback.text);
        assert!(fallback.choices.is_empty());

        let rows: Vec<Json> =
            (1..=25).map(|i| json!({"name": format!("Item {i}"), "sold": i * 1000, "ok": i % 2 == 0})).collect();
        let table = json!({"$ui": "table", "version": 1, "rows": rows, "columns": [
            {"key": "name", "label": "Name"}, {"key": "sold", "label": "Sold"}, {"key": "ok", "label": "OK"}, {"key": "none", "label": "Note"}]});
        let fallback = text(table, &TextOptions { max_rows: 2, ..TextOptions::default() });
        assert_eq!(
            fallback.text,
            "Name · Sold · OK · Note\n1. Item 1 · 1,000 · No · -\n2. Item 2 · 2,000 · Yes · -\n... and 23 more"
        );
    }

    #[test]
    fn stats_become_one_line_per_tile() {
        let stats = json!({"$ui": "stats", "version": 1, "title": "This week", "items": [
            {"label": "Revenue", "value": 125000000, "format": "currency", "currency": "IDR", "delta": 12.5, "delta_format": "percent", "caption": "vs last week"},
            {"label": "Refunds", "value": 3, "delta": -2},
            {"label": "Visitors", "value": 15320, "format": "compact", "delta": 0},
            {"label": "Status", "value": "Healthy"},
        ]});
        assert_eq!(
            text(stats, &TextOptions::whatsapp().with_separators('.', ',')).text,
            "*This week*\nRevenue: Rp 125.000.000 (▲ 12,5% vs last week)\nRefunds: 3 (▼ 2)\nVisitors: 15,3K (■ 0)\nStatus: Healthy"
        );
    }

    #[test]
    fn a_short_chart_lists_values_and_a_long_one_is_summarised() {
        let chart = json!({"$ui": "chart", "version": 1, "type": "bar", "title": "Sales",
            "x": {"values": ["Jan", "Feb"]}, "y": {"format": "currency", "currency": "USD"},
            "series": [{"name": "2025", "values": [1200.5, null]}, {"name": "2026", "values": [1500, 900]}]});
        assert_eq!(
            text(chart, &TextOptions::default()).text,
            "Sales\nJan: 2025 $1,200.5, 2026 $1,500\nFeb: 2025 -, 2026 $900"
        );

        let one = json!({"$ui": "chart", "version": 1, "type": "line", "x": {"values": ["Mon", "Tue"]},
            "series": [{"name": "Orders", "values": [12, 18]}]});
        assert_eq!(text(one, &TextOptions::default()).text, "Mon: 12\nTue: 18");

        let days: Vec<Json> = (1..=30).map(|day| json!(format!("Day {day}"))).collect();
        let values: Vec<Json> = (1..=30).map(|day| json!(day * 10)).collect();
        let long = json!({"$ui": "chart", "version": 1, "type": "line", "x": {"values": days},
            "series": [{"name": "Orders", "values": values}]});
        assert_eq!(
            text(long, &TextOptions::default()).text,
            "30 points, Day 1 to Day 30\nOrders: 10 to 300 (low 10, high 300)"
        );

        let pie = json!({"$ui": "chart", "version": 1, "type": "donut", "x": {"values": ["WhatsApp", "Email"]},
            "series": [{"name": "Chats", "values": [75, 25]}]});
        assert_eq!(text(pie, &TextOptions::default()).text, "WhatsApp: 75 (75%)\nEmail: 25 (25%)\nTotal: 100");
    }

    #[test]
    fn the_authors_fallback_text_replaces_the_generated_body() {
        let chart = json!({"$ui": "chart", "version": 1, "type": "bar", "title": "Sales", "fallback_text": "Sales grew 20% this month.",
            "x": {"values": ["Jan"]}, "series": [{"name": "A", "values": [1]}]});
        assert_eq!(text(chart, &TextOptions::whatsapp()).text, "*Sales*\nSales grew 20% this month.");
        assert_eq!(
            text(fixture("diagram-order-flow.json"), &TextOptions::default()).text,
            "How an order is handled\nFrom the first message to delivery\nThe bot checks stock, takes the order, and hands it to the owner to confirm before delivery."
        );
        let bare =
            json!({"$ui": "diagram", "version": 1, "syntax": "mermaid", "source": "sequenceDiagram\n A->>B: hi"});
        assert_eq!(text(bare, &TextOptions::default()).text, "[Sequence diagram]");
    }

    #[test]
    fn long_text_is_cut_but_choices_are_kept() {
        let card = json!({"$ui": "adaptive_card", "version": 1,
            "card": {"type": "AdaptiveCard", "version": "1.5",
                "body": [{"type": "TextBlock", "text": "word ".repeat(400)}],
                "actions": [{"type": "Action.Submit", "id": "ok", "title": "OK"}]}});
        let fallback = text(card, &TextOptions { max_chars: 200, ..TextOptions::default() });
        assert!(fallback.text.chars().count() <= 200, "{}", fallback.text.chars().count());
        assert!(fallback.text.contains("…\n\nReply with a number:\n1. OK"));
    }

    #[test]
    fn every_valid_fixture_has_a_text_form() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/valid");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let block: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let fallback = text(block, &TextOptions::default());
            assert!(!fallback.text.trim().is_empty(), "{}", path.display());
            assert!(!fallback.text.contains("$ui"), "{}", path.display());
        }
    }

    #[test]
    fn numbers_follow_their_format() {
        let en = TextOptions::default();
        let id = TextOptions::default().with_separators('.', ',');
        assert_eq!(format_number(1234567.891, None, None, &en), "1,234,567.89");
        assert_eq!(format_number(-0.004, None, None, &en), "0");
        assert_eq!(format_number(-1500.0, Some("currency"), Some("IDR"), &id), "-Rp 1.500");
        assert_eq!(format_number(19.99, Some("currency"), Some("EUR"), &id), "€19,99");
        assert_eq!(format_number(5.0, Some("currency"), Some("CHF"), &en), "CHF 5");
        assert_eq!(format_number(2_500_000.0, Some("compact"), None, &en), "2.5M");
        assert_eq!(format_number(950.0, Some("compact"), None, &en), "950");
        assert_eq!(format_number(12.5, Some("percent"), None, &en), "12.5%");
    }
}
