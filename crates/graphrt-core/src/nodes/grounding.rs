//! The `grounded` template filter: a reply may only state numbers and currencies its sources hold.
//!
//! `{{ reply | grounded([facts, message]) }}` gives the reply back when every number in it is
//! in one of the sources, and an empty text when one is not, so the template can say "I don't
//! know" or hand over instead of sending a made-up price. A currency the sources never name is
//! taken out of the reply rather than refusing it: a small model writes "$15k" or "120k KES"
//! for a laundry whose price list says "15k".
//!
//! Numbers are compared by value: `15k`, `15.000`, `15,000`, `15 ribu` and `15rb` are all
//! 15000, `1.5 jt` is 1500000. A clock time counts as its hour either way (`8pm` matches `20`,
//! `20:00` matches `8 pm`). The numbers 0, 1 and 2 without a currency count as words ("1 box",
//! "2 options"), not as facts.

use minijinja::value::{Value as JinjaValue, ValueKind};

/// Currencies by what a reply writes for them; the first name is the one compared.
const CURRENCIES: &[&[&str]] = &[
    &["usd", "$", "us$", "dollar", "dollars"],
    &["idr", "rp", "rupiah"],
    &["eur", "€", "euro", "euros"],
    &["gbp", "£"],
    &["jpy", "¥", "yen"],
    &["kes", "ksh"],
    &["sgd", "s$"],
    &["myr", "ringgit"],
    &["inr", "₹", "rupee", "rupees"],
    &["aud", "a$"],
    &["vnd", "₫", "dong"],
    &["thb", "฿", "baht"],
    &["php", "₱", "peso", "pesos"],
    &["cny", "rmb", "yuan"],
    &["krw", "₩", "won"],
    &["hkd", "hk$"],
    &["cad", "c$"],
    &["nzd", "nz$"],
    &["chf"],
    &["brl", "r$"],
    &["zar"],
    &["ngn", "₦", "naira"],
    &["aed", "dirham", "dirhams"],
    &["sar", "riyal", "riyals"],
    &["try", "₺", "lira"],
    &["mxn"],
    &["rub", "₽", "ruble", "rubles"],
    &["pkr"],
    &["bdt", "৳", "taka"],
    &["egp"],
    &["ghs", "cedi", "cedis"],
    &["ugx"],
    &["tzs"],
];

/// Multipliers a number may be written with, in the words that follow it.
const MULTIPLIERS: &[(&str, f64)] = &[
    ("k", 1_000.0),
    ("rb", 1_000.0),
    ("ribu", 1_000.0),
    ("thousand", 1_000.0),
    ("jt", 1_000_000.0),
    ("juta", 1_000_000.0),
    ("million", 1_000_000.0),
];

/// The `grounded` filter: `text` with currencies its sources do not name taken out, or `""`
/// when it states a number they do not hold. `sources` is a text or a list of texts.
pub(super) fn grounded(text: &str, sources: &JinjaValue) -> String {
    let source_text = source_texts(sources).join("\n");
    let known: Vec<f64> = numbers(&source_text).iter().flat_map(|number| number.values.clone()).collect();
    let known_currencies = currencies_in(&source_text);
    for number in numbers(text) {
        if number.is_a_word() {
            continue;
        }
        if !number.values.iter().any(|value| known.iter().any(|known| same(*known, *value))) {
            return String::new();
        }
    }
    without_unknown_currencies(text, &known_currencies)
}

fn source_texts(sources: &JinjaValue) -> Vec<String> {
    match sources.kind() {
        ValueKind::Seq => {
            sources.try_iter().map(|items| items.map(|item| item.to_string()).collect()).unwrap_or_default()
        }
        ValueKind::Undefined | ValueKind::None => Vec::new(),
        _ => vec![sources.to_string()],
    }
}

fn same(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-6
}

/// One number written in a text: the values it may mean, and whether it was written with a
/// currency, a multiplier or a fraction.
#[derive(Debug)]
struct Written {
    values: Vec<f64>,
    plain_small: bool,
}

impl Written {
    fn is_a_word(&self) -> bool {
        self.plain_small
    }
}

/// Lowercase words of `text` (letters, digits and currency signs), in order.
fn tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, tokens: &mut Vec<String>| {
        if !word.is_empty() {
            tokens.push(std::mem::take(word));
        }
    };
    let characters: Vec<char> = text.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        let between_digits = |at: usize| {
            at > 0
                && characters.get(at - 1).is_some_and(char::is_ascii_digit)
                && characters.get(at + 1).is_some_and(char::is_ascii_digit)
        };
        match character {
            c if c.is_alphanumeric() => {
                // `15k`, `8pm`: a number and the letters after it are separate words.
                let switches = word.chars().last().is_some_and(|last| last.is_ascii_digit() != c.is_ascii_digit());
                if switches && !word.ends_with([':', '.', ',']) {
                    flush(&mut word, &mut tokens);
                }
                word.extend(c.to_lowercase());
            }
            '.' | ',' | ':' if between_digits(index) => word.push(*character),
            '$' | '€' | '£' | '¥' | '₹' | '₫' | '฿' | '₱' | '₩' | '₦' | '₺' | '₽' | '৳' => {
                // `US$`, `S$`, `A$`: the letters before belong to the sign.
                if !matches!(word.as_str(), "us" | "s" | "a" | "hk" | "c" | "nz" | "r") {
                    flush(&mut word, &mut tokens);
                }
                word.push(*character);
                flush(&mut word, &mut tokens);
            }
            _ => flush(&mut word, &mut tokens),
        }
    }
    flush(&mut word, &mut tokens);
    tokens
}

fn numbers(text: &str) -> Vec<Written> {
    let tokens = tokens(text);
    let mut found = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let next = tokens.get(index + 1).map(String::as_str);
        let before = index.checked_sub(1).and_then(|before| tokens.get(before)).map(String::as_str);
        let with_currency = [before, next, tokens.get(index + 2).map(String::as_str)]
            .into_iter()
            .flatten()
            .any(|word| currency_of(word).is_some());
        let mut values = Vec::new();
        if let Some((hour, minutes)) = token.split_once(':') {
            values.extend(hour.parse::<f64>().ok());
            if let Ok(minutes) = minutes.parse::<f64>()
                && minutes > 0.0
            {
                found.push(Written { values: vec![minutes], plain_small: false });
            }
        } else if let Some(value) = parse_number(token) {
            let multiplier =
                next.and_then(|next| MULTIPLIERS.iter().find(|(word, _)| *word == next)).map(|(_, by)| *by);
            values.push(value * multiplier.unwrap_or(1.0));
            let plain_small = multiplier.is_none() && !with_currency && !token.contains(['.', ',']) && value <= 2.0;
            if plain_small {
                found.push(Written { values, plain_small });
                continue;
            }
        }
        // A clock time: its hour on the other half of the day too.
        if matches!(next, Some("am" | "pm" | "pagi" | "sore" | "malam")) || token.contains(':') {
            let hours: Vec<f64> = values.iter().filter(|hour| **hour < 24.0).map(|hour| (hour + 12.0) % 24.0).collect();
            values.extend(hours);
        }
        if !values.is_empty() {
            found.push(Written { values, plain_small: false });
        }
    }
    found
}

/// `15.000` and `15,000` as fifteen thousand, `1.5` and `1,5` as one and a half.
fn parse_number(token: &str) -> Option<f64> {
    let groups: Vec<&str> = token.split(['.', ',']).collect();
    let thousands = groups.len() > 1 && groups.iter().skip(1).all(|group| group.len() == 3);
    if thousands {
        return groups.concat().parse().ok();
    }
    match groups.as_slice() {
        [whole] => whole.parse().ok(),
        [whole, fraction] => format!("{whole}.{fraction}").parse().ok(),
        _ => None,
    }
}

fn currency_of(word: &str) -> Option<&'static str> {
    CURRENCIES.iter().find(|names| names.contains(&word)).and_then(|names| names.first().copied())
}

fn currencies_in(text: &str) -> Vec<&'static str> {
    tokens(text).iter().filter_map(|word| currency_of(word)).collect()
}

/// `text` without the currency words and signs of currencies `known` does not hold.
fn without_unknown_currencies(text: &str, known: &[&str]) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some((start, length)) = next_currency_mark(rest, known) else {
            kept.push_str(rest);
            break;
        };
        kept.push_str(&rest[..start]);
        rest = &rest[start + length..];
        // One space is enough where the mark was: "costs $15k" stays "costs 15k".
        if kept.ends_with(' ') && rest.starts_with(' ') {
            rest = &rest[1..];
        }
    }
    let kept = kept.replace(" .", ".").replace(" ,", ",");
    kept.trim().to_owned()
}

/// Where the next mark of an unknown currency is in `text` (byte offset and length).
fn next_currency_mark(text: &str, known: &[&str]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for names in CURRENCIES {
        if names.first().is_some_and(|name| known.contains(name)) {
            continue;
        }
        for name in names.iter() {
            let is_sign = !name.chars().all(char::is_alphabetic);
            let lower = text.to_lowercase();
            // Lowercasing keeps byte offsets for the ASCII and sign characters searched here.
            if lower.len() != text.len() {
                continue;
            }
            let mut from = 0;
            while let Some(found) = lower[from..].find(name) {
                let start = from + found;
                let end = start + name.len();
                // A currency's name or code only next to a number: "try again" is no lira.
                let whole_word = is_sign
                    || (!lower[..start].chars().last().is_some_and(char::is_alphanumeric)
                        && !lower[end..].chars().next().is_some_and(char::is_alphabetic)
                        && next_to_a_number(&lower[..start], &lower[end..]));
                if whole_word && best.is_none_or(|(at, _)| start < at) {
                    best = Some((start, name.len()));
                }
                from = end;
            }
        }
    }
    best
}

/// Whether a word between `before` and `after` stands next to a number (`120k VND`, `IDR 25.000`).
fn next_to_a_number(before: &str, after: &str) -> bool {
    let before = before.trim_end();
    let before = MULTIPLIERS
        .iter()
        .find_map(|(word, _)| {
            before.strip_suffix(word).filter(|rest| rest.ends_with(|c: char| c.is_ascii_digit() || c == ' '))
        })
        .unwrap_or(before)
        .trim_end();
    before.ends_with(|c: char| c.is_ascii_digit()) || after.trim_start().starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(texts: &[&str]) -> JinjaValue {
        JinjaValue::from(texts.iter().map(|text| JinjaValue::from(*text)).collect::<Vec<_>>())
    }

    const LAUNDRY: &str =
        "A chatbot for my laundry shop: prices are 8k per kg, express 15k per kg, pickup between 9 and 18.";

    #[test]
    fn a_currency_the_facts_never_name_is_taken_out() {
        let facts = sources(&[LAUNDRY, "How much is express laundry?"]);
        assert_eq!(grounded("$15k per kg.", &facts), "15k per kg.");
        assert_eq!(
            grounded("Our express laundry service costs $15,000 per kilogram.", &facts),
            "Our express laundry service costs 15,000 per kilogram."
        );
        assert_eq!(grounded("Cheesecakes are 120k KES.", &sources(&["cheesecake 120k"])), "Cheesecakes are 120k.");
        assert_eq!(
            grounded("Our cheesecakes are priced at 120k VND.", &sources(&["cheesecake 120k"])),
            "Our cheesecakes are priced at 120k."
        );
        assert_eq!(grounded("Price: ₱120k.", &sources(&["cheesecake 120k"])), "Price: 120k.");
        // A currency's word away from a number is a word.
        assert_eq!(
            grounded("Please try again, we won a prize.", &sources(&["x"])),
            "Please try again, we won a prize."
        );
        assert_eq!(
            grounded("Harga nasi box Rp 25.000 per box.", &sources(&["nasi box 25k"])),
            "Harga nasi box 25.000 per box."
        );
        // A currency the facts name stays.
        assert_eq!(grounded("Nasi box Rp 25.000.", &sources(&["nasi box Rp25.000"])), "Nasi box Rp 25.000.");
        assert_eq!(grounded("It costs 25,000 IDR.", &sources(&["25 ribu rupiah"])), "It costs 25,000 IDR.");
    }

    #[test]
    fn a_number_the_sources_do_not_hold_empties_the_reply() {
        let facts = sources(&[LAUNDRY, "How much is express laundry?"]);
        assert_eq!(grounded("Express costs 20k per kg.", &facts), "");
        assert_eq!(grounded("We open at 7am.", &facts), "");
        // The same value written another way is the same number.
        assert_eq!(
            grounded("Express is 15.000 per kg, regular 8 ribu.", &facts),
            "Express is 15.000 per kg, regular 8 ribu."
        );
        assert_eq!(grounded("Pickup is from 9am to 6pm.", &facts), "Pickup is from 9am to 6pm.");
        assert_eq!(grounded("Pickup until 18:00.", &facts), "Pickup until 18:00.");
        // Numbers the customer wrote may be repeated, small counting words are words.
        let asked = sources(&[LAUNDRY, "I have 5 kg, how much?"]);
        assert_eq!(
            grounded("For 5 kg that is 2 options: 8k or 15k per kg.", &asked),
            "For 5 kg that is 2 options: 8k or 15k per kg."
        );
        assert_eq!(grounded("Yes, 1 kg is fine.", &facts), "Yes, 1 kg is fine.");
        assert_eq!(grounded("That is $2.", &facts), "");
    }

    #[test]
    fn numbers_are_read_in_the_ways_people_write_them() {
        let values = |text: &str| numbers(text).into_iter().flat_map(|number| number.values).collect::<Vec<_>>();
        assert_eq!(values("15k"), [15_000.0]);
        assert_eq!(values("Rp 1.500.000"), [1_500_000.0]);
        assert_eq!(values("1,5 jt"), [1_500_000.0]);
        assert_eq!(values("120k"), [120_000.0]);
        assert_eq!(values("8pm"), [8.0, 20.0]);
        assert_eq!(values("9-20"), [9.0, 20.0]);
        assert!(values("Monday to Saturday").is_empty());
        assert_eq!(grounded("", &sources(&["x"])), "");
        assert_eq!(
            grounded("Open Monday to Saturday.", &JinjaValue::from("closed Sunday")),
            "Open Monday to Saturday."
        );
    }
}
