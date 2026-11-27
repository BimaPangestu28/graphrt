//! Estimating the size of a conversation and trimming it: pure functions over
//! the messages, no model and no I/O.
//!
//! Sizes are estimates in tokens: there is no tokenizer here. The caller
//! corrects them with what the provider reports (see [`super::ContextState`]).

use graphrt_core::tool::ToolSpec;
use serde_json::Value as Json;

use crate::types::{ContentPart, Message, Role};

/// Estimated tokens of one message beyond its content (role, separators).
const MESSAGE_OVERHEAD: u64 = 4;
/// Estimated tokens of one image. Providers charge between a few hundred and
/// a few thousand depending on size and detail.
const IMAGE_TOKENS: u64 = 1_000;
/// Tool results shorter than this (in bytes) are never replaced: the
/// placeholder would cost about as much.
const MIN_PLACEHOLDER_BYTES: usize = 200;
/// Start of the text that replaces a removed tool result.
const PLACEHOLDER_PREFIX: &str = "[tool result removed to save context";
/// How the marker in a cut tool result ends.
const CUT_MARKER: &str = "characters removed to fit the context window]";
/// The shortest text the last resort leaves of a tool result (in characters).
const MIN_CUT_CHARS: usize = 400;

/// Estimated tokens of `text`: ASCII bytes at 3.5 per token, every other
/// character as one token (CJK text is close to one token per character).
pub(crate) fn text_tokens(text: &str) -> u64 {
    /// The top bit of every byte of a word.
    const HIGH: u64 = 0x8080_8080_8080_8080;
    // Eight bytes at a time: a byte with its top bit set is not ASCII, and one
    // with its two top bits set starts a character.
    let (words, rest) = text.as_bytes().as_chunks::<8>();
    let (mut not_ascii, mut starts) = (0u64, 0u64);
    for word in words {
        let word = u64::from_le_bytes(*word);
        not_ascii += u64::from((word & HIGH).count_ones());
        starts += u64::from((word & (word << 1) & HIGH).count_ones());
    }
    for byte in rest {
        not_ascii += u64::from(*byte >= 0x80);
        starts += u64::from(*byte >= 0xC0);
    }
    let ascii = text.len() as u64 - not_ascii;
    (ascii * 2).div_ceil(7) + starts
}

/// Estimated tokens of one message.
pub(crate) fn message_tokens(message: &Message) -> u64 {
    let mut tokens = MESSAGE_OVERHEAD;
    for part in &message.content {
        tokens += match part {
            ContentPart::Text { text } => text_tokens(text),
            ContentPart::Image { .. } => IMAGE_TOKENS,
        };
    }
    for call in &message.tool_calls {
        tokens += MESSAGE_OVERHEAD + text_tokens(&call.name) + text_tokens(&call.arguments);
    }
    if let Some(state) = &message.provider_state {
        tokens += json_bytes(state).div_ceil(3) as u64;
    }
    tokens
}

/// Estimated tokens of a conversation.
pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// Estimated tokens of the tool definitions sent with a request.
pub fn estimate_tools(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            MESSAGE_OVERHEAD
                + text_tokens(&tool.name)
                + text_tokens(&tool.description)
                + json_bytes(&tool.parameters).div_ceil(3) as u64
        })
        .sum()
}

/// Roughly the length of `value` as compact JSON, without building the text.
fn json_bytes(value: &Json) -> usize {
    match value {
        Json::Null | Json::Bool(_) => 5,
        Json::Number(_) => 8,
        Json::String(text) => text.len() + 2,
        Json::Array(items) => 2 + items.iter().map(|item| json_bytes(item) + 1).sum::<usize>(),
        Json::Object(map) => 2 + map.iter().map(|(key, item)| key.len() + 4 + json_bytes(item)).sum::<usize>(),
    }
}

/// What [`trim`] keeps whatever the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keep {
    /// The newest tool results that are never replaced by a placeholder.
    pub tool_results: usize,
    /// The newest groups that are never removed and whose tool results are
    /// never replaced. 0 is treated as 1: the turn in progress always stays.
    pub turns: usize,
}

/// What the caller knows must stay, beyond [`Keep`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scope<'a> {
    /// Tool results that are never replaced, cut or removed (for example the
    /// instructions of a loaded skill), each as the id of its call and its
    /// text as the model was shown it. Both must match: providers reuse call
    /// ids from one turn to the next, so an id alone does not name one result.
    pub pinned: &'a [(&'a str, &'a str)],
    /// The last messages of the conversation that are never removed (for
    /// example the messages of a prompt template after the history).
    pub tail: usize,
}

/// What one [`trim`] did.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Trimmed {
    /// Estimated tokens before.
    pub before: u64,
    /// Estimated tokens after.
    pub after: u64,
    /// Tool results replaced by a placeholder in messages that stay.
    pub placeholders: usize,
    /// Tool results cut in the middle.
    pub cut: usize,
    /// The messages that were removed, oldest first, as they were before any
    /// placeholder was written (so they can still be summarized).
    pub removed: Vec<Message>,
    /// The tool results that became placeholders in messages that stay,
    /// oldest first, as they were (so they can still be summarized).
    pub replaced: Vec<Message>,
}

impl Trimmed {
    /// Whether the conversation changed at all.
    pub fn changed(&self) -> bool {
        self.placeholders > 0 || self.cut > 0 || !self.removed.is_empty()
    }
}

/// A run of messages that is kept or removed as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Group {
    start: usize,
    /// One past the last message.
    end: usize,
    /// Never removed.
    protected: bool,
    /// One of the newest groups: its tool results are never replaced either.
    fresh: bool,
    /// Starts with a user message (a turn of the conversation), as opposed to
    /// a model turn that called tools.
    user_led: bool,
}

/// What [`trim`] keeps beside each message while it works. The vectors stay
/// aligned with the messages: a message that is removed takes its entries along.
struct Work {
    /// Never replaced, cut or removed (see [`Scope::pinned`]).
    pinned: Vec<bool>,
    /// Where the message stood before the trim.
    origin: Vec<usize>,
    /// What the message was before this trim made it a placeholder.
    original: Vec<Option<Message>>,
    /// One of the last `Scope::tail` messages.
    tail_start: usize,
    /// Removed messages with where they stood.
    removed: Vec<(usize, Message)>,
    cut: usize,
}

impl Work {
    fn new(messages: &[Message], scope: Scope<'_>) -> Self {
        // One message per pinned result, the newest that matches: when the same
        // text was returned twice, the older copy may go.
        let mut pinned = vec![false; messages.len()];
        for (id, text) in scope.pinned {
            let found = messages.iter().rposition(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some(*id)
                    && matches!(message.content.as_slice(), [ContentPart::Text { text: own }] if own == text)
            });
            if let Some(flag) = found.and_then(|at| pinned.get_mut(at)) {
                *flag = true;
            }
        }
        Self {
            pinned,
            origin: (0..messages.len()).collect(),
            original: messages.iter().map(|_| None).collect(),
            tail_start: messages.len().saturating_sub(scope.tail),
            removed: Vec::new(),
            cut: 0,
        }
    }

    fn is_pinned(&self, index: usize) -> bool {
        self.pinned.get(index).copied().unwrap_or(false)
    }

    /// Whether the message at `index` now is one of the protected last messages.
    fn in_tail(&self, index: usize) -> bool {
        self.origin.get(index).is_some_and(|origin| *origin >= self.tail_start)
    }
}

/// Split `messages` into groups: a user message with the plain answers after
/// it, or an assistant message that calls tools with every tool result after
/// it. Removing a whole group never parts a tool call from its results.
///
/// Protected: system messages, the group of the last user message (the
/// request being answered), the newest `keep_turns` groups, groups that reach
/// into the tail and groups with a pinned result.
fn groups(messages: &[Message], keep_turns: usize, work: &Work) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut last_user_group = None;
    let mut index = 0;
    while let Some(message) = messages.get(index) {
        let start = index;
        index += 1;
        let follows = |m: &Message| match (message.role, message.tool_calls.is_empty()) {
            // A user message: the plain answers after it.
            (Role::User, _) => m.role == Role::Assistant && m.tool_calls.is_empty(),
            // A turn that calls tools (or a stray result): the results after it.
            (Role::Assistant, false) | (Role::Tool, _) => m.role == Role::Tool,
            (Role::System, _) | (Role::Assistant, true) => false,
        };
        while messages.get(index).is_some_and(&follows) {
            index += 1;
        }
        if message.role == Role::User {
            last_user_group = Some(out.len());
        }
        // A tool result without its call in front (never produced by the nodes)
        // joins the group before it, so it cannot be left behind alone.
        if message.role == Role::Tool
            && let Some(previous) = out.last_mut()
        {
            previous.end = index;
            continue;
        }
        out.push(Group {
            start,
            end: index,
            protected: message.role == Role::System,
            fresh: false,
            user_led: message.role == Role::User,
        });
    }
    let newest = out.len().saturating_sub(keep_turns.max(1));
    for (position, group) in out.iter_mut().enumerate() {
        group.fresh = position >= newest;
        let held = (group.start..group.end).any(|at| work.is_pinned(at) || work.in_tail(at));
        group.protected |= group.fresh || Some(position) == last_user_group || held;
    }
    out
}

/// Whether `message` is a placeholder written by [`placeholder`]. The exact
/// shape is checked, so a tool result that merely starts like one is still
/// replaced and cut like any other.
fn is_placeholder(message: &Message) -> bool {
    matches!(
        message.content.as_slice(),
        [ContentPart::Text { text }] if text.len() <= 100 && text.starts_with(PLACEHOLDER_PREFIX) && text.ends_with(']')
    )
}

fn placeholder(message: &Message) -> Message {
    let chars: usize = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.chars().count(),
            ContentPart::Image { .. } => 0,
        })
        .sum();
    let images = message.images().count();
    let text = match images {
        0 => format!("{PLACEHOLDER_PREFIX}: {chars} characters]"),
        _ => format!("{PLACEHOLDER_PREFIX}: {chars} characters, {images} image(s)]"),
    };
    Message { content: vec![ContentPart::Text { text }], ..message.clone() }
}

fn content_bytes(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.len(),
            // An image is always worth replacing.
            ContentPart::Image { .. } => MIN_PLACEHOLDER_BYTES,
        })
        .sum()
}

/// Trim `messages` until their estimate is at most `target` (never more than
/// `limit`), in steps. Each step runs only while the estimate is still too large:
///
/// 1. Old tool results become a placeholder: oldest first, never the newest
///    `keep.tool_results`, never a result of the newest `keep.turns` groups
///    (the model may not have read it yet), never results under 200 bytes.
/// 2. The oldest model turns that called tools are removed whole.
/// 3. A tool result that alone takes more than a quarter of `limit` is cut
///    in the middle to that size.
/// 4. If the estimate is still above `limit`, the largest tool results that
///    are left are cut in the middle, to a tenth under `limit`.
/// 5. If the estimate is still above `limit`, the oldest turns of the
///    conversation (a user message and its answer) are removed whole. They go
///    last because they hold what the user asked for: tool output, however
///    large, costs tool output first.
///
/// Whole groups are removed (see [`groups`]), so a tool call never loses its
/// result. Nothing happens when the estimate is already at most `limit`. The
/// result says what was done. `after` above `limit` means nothing else can be
/// removed.
pub fn trim(messages: &mut Vec<Message>, target: u64, limit: u64, keep: Keep, scope: Scope<'_>) -> Trimmed {
    let target = target.min(limit);
    let before = estimate_messages(messages);
    let mut done = Trimmed { before, after: before, ..Trimmed::default() };
    if before <= limit {
        return done;
    }
    let mut work = Work::new(messages, scope);
    let mut total = before;
    total = replace_old_results(messages, total, target, keep, &mut work);
    total = remove_groups(messages, total, target, keep, false, &mut work);
    if total > target {
        total = cut_results(messages, total, target, Some(limit / 4), &mut work);
    }
    if total > limit {
        // A tenth under the limit, so the next turn has room before the next trim.
        total = cut_results(messages, total, limit - limit / 10, None, &mut work);
    }
    if total > limit {
        total = remove_groups(messages, total, target, keep, true, &mut work);
    }
    done.after = total;
    done.cut = work.cut;
    // Oldest first, whatever the order of the steps.
    work.removed.sort_by_key(|(origin, _)| *origin);
    done.removed = work.removed.into_iter().map(|(_, message)| message).collect();
    done.replaced = work.original.into_iter().flatten().collect();
    done.placeholders = done.replaced.len();
    done
}

/// Step 1 of [`trim`]. Returns the new estimate.
fn replace_old_results(messages: &mut [Message], mut total: u64, target: u64, keep: Keep, work: &mut Work) -> u64 {
    let mut fresh = vec![false; messages.len()];
    for group in groups(messages, keep.turns, work).iter().filter(|group| group.fresh) {
        for flag in fresh.get_mut(group.start..group.end).unwrap_or_default() {
            *flag = true;
        }
    }
    let results = messages.iter().filter(|message| message.role == Role::Tool).count();
    let mut old = results.saturating_sub(keep.tool_results);
    for (index, message) in messages.iter_mut().enumerate() {
        if total <= target || old == 0 {
            break;
        }
        if message.role != Role::Tool {
            continue;
        }
        old -= 1;
        let stays = fresh.get(index).copied().unwrap_or(true)
            || work.is_pinned(index)
            || work.in_tail(index)
            || is_placeholder(message)
            || content_bytes(message) < MIN_PLACEHOLDER_BYTES;
        if stays {
            continue;
        }
        let size = message_tokens(message);
        let shorter = placeholder(message);
        let smaller = message_tokens(&shorter);
        if smaller >= size {
            continue;
        }
        if let Some(slot) = work.original.get_mut(index) {
            *slot = Some(std::mem::replace(message, shorter));
            total -= size - smaller;
        }
    }
    total
}

/// Steps 2 and 5 of [`trim`]: remove the oldest unprotected groups, those
/// led by a user message (`user_led`) or the others. Returns the new estimate.
fn remove_groups(
    messages: &mut Vec<Message>,
    mut total: u64,
    target: u64,
    keep: Keep,
    user_led: bool,
    work: &mut Work,
) -> u64 {
    if total <= target {
        return total;
    }
    let mut removed = vec![false; messages.len()];
    for group in groups(messages, keep.turns, work) {
        if total <= target {
            break;
        }
        if group.protected || group.user_led != user_led {
            continue;
        }
        let size = estimate_messages(messages.get(group.start..group.end).unwrap_or_default());
        total = total.saturating_sub(size);
        for flag in removed.get_mut(group.start..group.end).unwrap_or_default() {
            *flag = true;
        }
    }
    if !removed.contains(&true) {
        return total;
    }
    // The messages and everything kept beside them move together.
    let old = Work {
        pinned: std::mem::take(&mut work.pinned),
        origin: std::mem::take(&mut work.origin),
        original: std::mem::take(&mut work.original),
        tail_start: work.tail_start,
        removed: Vec::new(),
        cut: 0,
    };
    let entries = std::mem::take(messages).into_iter().zip(old.pinned).zip(old.origin).zip(old.original).zip(removed);
    for ((((message, pinned), origin), original), gone) in entries {
        if gone {
            // What is handed back for a summary is the result as it was, not its placeholder.
            work.removed.push((origin, original.unwrap_or(message)));
        } else {
            messages.push(message);
            work.pinned.push(pinned);
            work.origin.push(origin);
            work.original.push(original);
        }
    }
    total
}

/// Steps 3 and 4 of [`trim`]: cut tool results in the middle, the largest
/// first, until the estimate is at most `target` or nothing is left to cut.
/// With `max_tokens`, only results larger than that are cut, and to that
/// size. Without it a result is cut as far as needed, down to 400
/// characters. Returns the new estimate.
fn cut_results(messages: &mut [Message], mut total: u64, target: u64, max_tokens: Option<u64>, work: &mut Work) -> u64 {
    let mut candidates: Vec<(usize, u64)> = messages
        .iter()
        .enumerate()
        .filter(|(index, message)| message.role == Role::Tool && !is_placeholder(message) && !work.is_pinned(*index))
        .map(|(index, message)| (index, message_tokens(message)))
        .collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    for (index, size) in candidates {
        if total <= target {
            break;
        }
        let Some(message) = messages.get_mut(index) else { continue };
        let text = message.text();
        let chars = text.chars().count();
        let keep_chars = match max_tokens {
            Some(max_tokens) if size <= max_tokens => continue,
            // The same share of its characters as of its tokens, whatever the script.
            Some(max_tokens) => {
                let share = (chars as u64).saturating_mul(max_tokens) / size.max(1);
                usize::try_from(share).unwrap_or(chars).max(MIN_CUT_CHARS)
            }
            None => {
                // Tokens still to save, as characters, with room for the marker.
                let needed = usize::try_from((total - target).saturating_mul(4)).unwrap_or(usize::MAX);
                chars.saturating_sub(needed.saturating_add(80)).max(MIN_CUT_CHARS)
            }
        };
        if keep_chars >= chars {
            continue;
        }
        // Images of a result that has to be cut are dropped with the middle, and the marker says so.
        let images = message.images().count();
        let mut shorter_text = cut_middle(&text, chars, keep_chars);
        if images > 0 {
            shorter_text.push_str(&format!("\n[{images} image(s) removed to fit the context window]"));
        }
        let shorter = Message { content: vec![ContentPart::Text { text: shorter_text }], ..message.clone() };
        let smaller = message_tokens(&shorter);
        // The marker has a size too: a cut that saves nothing is not made.
        if smaller >= size {
            continue;
        }
        // A result that is cut a second time counts once.
        if !text.contains(CUT_MARKER) {
            work.cut += 1;
        }
        *message = shorter;
        total -= size - smaller;
    }
    total
}

/// `text` (of `chars` characters) with its middle replaced by a marker, so
/// that about `keep_chars` characters remain: the start and the end of a
/// result are usually what matters.
pub(crate) fn cut_middle(text: &str, chars: usize, keep_chars: usize) -> String {
    if keep_chars >= chars {
        return text.to_owned();
    }
    let head = keep_chars / 2;
    let tail = keep_chars - head;
    let head_end = text.char_indices().nth(head).map_or(text.len(), |(offset, _)| offset);
    let tail_start = text.char_indices().nth(chars - tail).map_or(text.len(), |(offset, _)| offset);
    let dropped = chars - keep_chars;
    format!(
        "{}\n[{dropped} {CUT_MARKER}\n{}",
        text.get(..head_end).unwrap_or_default(),
        text.get(tail_start..).unwrap_or_default()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::types::ToolCall;

    fn call(id: &str) -> Message {
        Message {
            tool_calls: vec![ToolCall { id: id.into(), name: "search".into(), arguments: "{}".into() }],
            ..Message::assistant("")
        }
    }

    /// system, a history turn, the request, then `turns` tool turns of one
    /// call each with a result of `result_chars` characters.
    fn conversation(turns: usize, result_chars: usize) -> Vec<Message> {
        let mut messages = vec![
            Message::system("You are helpful."),
            Message::user("earlier question"),
            Message::assistant("earlier answer"),
            Message::user("the request"),
        ];
        for turn in 0..turns {
            messages.push(call(&format!("c{turn}")));
            messages.push(Message::tool(format!("c{turn}"), "x".repeat(result_chars)));
        }
        messages
    }

    /// Every tool result has its call right before the run of results it is
    /// in, and every call has all its results.
    fn assert_pairs_intact(messages: &[Message]) {
        let mut open: Vec<String> = Vec::new();
        for message in messages {
            match message.role {
                Role::Tool => {
                    let id = message.tool_call_id.clone().unwrap();
                    assert!(open.contains(&id), "tool result {id} has no call before it");
                    open.retain(|other| other != &id);
                }
                _ => {
                    assert!(open.is_empty(), "calls {open:?} have no result");
                    open = message.tool_calls.iter().map(|call| call.id.clone()).collect();
                }
            }
        }
        assert!(open.is_empty(), "calls {open:?} have no result");
    }

    /// Like [`assert_pairs_intact`] for conversations whose call ids repeat:
    /// every run of results follows an assistant message with as many calls.
    fn assert_pairs_intact_by_position(messages: &[Message]) {
        let mut open = 0;
        for message in messages {
            if message.role == Role::Tool {
                assert!(open > 0, "a result without a call before it");
                open -= 1;
            } else {
                assert_eq!(open, 0, "calls without results");
                open = message.tool_calls.len();
            }
        }
        assert_eq!(open, 0, "calls without results");
    }

    const KEEP: Keep = Keep { tool_results: 2, turns: 2 };

    #[test]
    fn text_is_estimated_by_bytes_and_by_characters() {
        assert_eq!(text_tokens(""), 0);
        assert_eq!(text_tokens(&"a".repeat(35)), 10);
        // One token per character outside ASCII, whatever its width in bytes.
        assert_eq!(text_tokens("日本語"), 3);
        assert_eq!(text_tokens("é"), 1);
        assert_eq!(text_tokens("😀😀"), 2);
    }

    #[test]
    fn a_message_counts_text_images_calls_and_provider_state() {
        let plain = message_tokens(&Message::user("a".repeat(35)));
        assert_eq!(plain, MESSAGE_OVERHEAD + 10);
        let mut with_image = Message::user("a".repeat(35));
        with_image.content.push(ContentPart::Image { url: "https://x/y.png".into(), detail: None });
        assert_eq!(message_tokens(&with_image), plain + IMAGE_TOKENS);
        assert!(message_tokens(&call("c1")) > MESSAGE_OVERHEAD);
        let mut stateful = Message::assistant("");
        stateful.provider_state = Some(serde_json::json!({"signature": "s".repeat(300)}));
        assert!(message_tokens(&stateful) > 100);
    }

    #[test]
    fn tool_definitions_are_estimated_from_their_schema() {
        let small = ToolSpec { name: "a".into(), description: "b".into(), parameters: serde_json::json!({}) };
        let large = ToolSpec {
            name: "a".into(),
            description: "b".repeat(700),
            parameters: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        };
        assert!(estimate_tools(&[large]) > estimate_tools(&[small]) + 200);
        assert_eq!(estimate_tools(&[]), 0);
    }

    #[test]
    fn nothing_changes_under_the_limit() {
        let mut messages = conversation(3, 1_000);
        let original = messages.clone();
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size / 2, size, KEEP, Scope::default());
        assert!(!done.changed());
        assert_eq!((done.before, done.after), (size, size));
        assert_eq!(messages, original);
    }

    #[test]
    fn old_tool_results_become_placeholders_first() {
        let mut messages = conversation(6, 7_000);
        let size = estimate_messages(&messages);
        // Two results have to go: placeholders are enough, nothing is removed.
        let done = trim(&mut messages, size - 3_000, size - 1, KEEP, Scope::default());
        assert_eq!(done.placeholders, 2);
        assert!(done.removed.is_empty());
        // What was replaced is handed back in full, for a summary.
        assert_eq!(done.replaced.len(), 2);
        assert_eq!(done.replaced[0].text().len(), 7_000);
        assert_eq!(done.replaced[0].tool_call_id.as_deref(), Some("c0"));
        assert_eq!(messages.len(), 4 + 12);
        assert_eq!(done.after, estimate_messages(&messages));
        assert!(done.after <= size - 3_000);
        let texts: Vec<String> = messages.iter().filter(|m| m.role == Role::Tool).map(Message::text).collect();
        assert_eq!(texts[0], "[tool result removed to save context: 7000 characters]");
        assert!(texts[1].starts_with(PLACEHOLDER_PREFIX));
        assert_eq!(texts[2].len(), 7_000, "the oldest results go first");
        // The message still answers its call.
        assert_eq!(messages[5].tool_call_id.as_deref(), Some("c0"));
        assert_pairs_intact(&messages);
    }

    #[test]
    fn the_newest_tool_results_and_small_ones_are_never_replaced() {
        let mut messages = conversation(4, 7_000);
        // An old result that is too small to be worth a placeholder.
        messages[5] = Message::tool("c0", "ok");
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size - 1_500, size - 1, Keep { tool_results: 2, turns: 1 }, Scope::default());
        let texts: Vec<String> = messages.iter().filter(|m| m.role == Role::Tool).map(Message::text).collect();
        assert_eq!(texts[0], "ok");
        assert!(texts[1].starts_with(PLACEHOLDER_PREFIX));
        // The newest two are never placeholders.
        assert_eq!((texts[2].len(), texts[3].len()), (7_000, 7_000));
        assert_eq!((done.placeholders, done.cut, done.removed.len()), (1, 0, 0));
    }

    #[test]
    fn a_placeholder_is_not_replaced_again() {
        let mut messages = conversation(6, 7_000);
        let size = estimate_messages(&messages);
        trim(&mut messages, size - 3_000, size - 1, KEEP, Scope::default());
        let after_first = messages.clone();
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size - 1_500, size - 1, KEEP, Scope::default());
        assert_eq!(done.placeholders, 1, "only one more result is replaced");
        assert_eq!(messages[5], after_first[5]);
    }

    #[test]
    fn images_of_an_old_tool_result_go_with_it() {
        let mut messages = conversation(4, 10);
        messages[5] = Message::tool_with_images("c0", "shot", ["data:image/png;base64,AAAA".to_owned()]);
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size - 500, size - 1, KEEP, Scope::default());
        assert_eq!(done.placeholders, 1);
        assert_eq!(messages[5].text(), "[tool result removed to save context: 4 characters, 1 image(s)]");
        assert_eq!(messages[5].images().count(), 0);
    }

    #[test]
    fn the_oldest_groups_are_removed_whole() {
        let mut messages = conversation(6, 7_000);
        let done = trim(&mut messages, 2_500, 2_500, KEEP, Scope::default());
        // The four oldest tool turns are gone. The turn of the conversation before the
        // request stays: the two results that are left were cut to a quarter of the limit instead.
        assert_eq!(done.removed.len(), 8);
        assert_eq!(done.removed[0].tool_calls[0].id, "c0");
        // Removed results are handed back with their text, not as placeholders, for the summary.
        assert_eq!(done.removed[1].text().len(), 7_000);
        assert!(done.replaced.is_empty() && done.placeholders == 0, "what was replaced and then removed counts once");
        assert_eq!(done.cut, 2);
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        let expected = [
            Role::System,
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::Tool,
        ];
        assert_eq!(roles, expected);
        assert_eq!(messages[1].text(), "earlier question");
        assert_eq!(messages[3].text(), "the request");
        assert!(done.after <= 2_500);
        assert_eq!(done.after, estimate_messages(&messages));
        assert_pairs_intact(&messages);
    }

    #[test]
    fn turns_of_the_conversation_go_last_and_come_back_first() {
        let mut messages = conversation(6, 7_000);
        // Too small for the tool traffic that is kept: the earlier turn has to go too.
        let done = trim(&mut messages, 300, 300, KEEP, Scope::default());
        assert_eq!(done.removed.len(), 2 + 8);
        // Oldest first, whatever the order they were removed in.
        assert_eq!(done.removed[0].text(), "earlier question");
        assert_eq!(done.removed[2].tool_calls[0].id, "c0");
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User, Role::Assistant, Role::Tool, Role::Assistant, Role::Tool]);
        assert_eq!(messages[1].text(), "the request");
        assert_eq!(done.after, estimate_messages(&messages));
        assert_pairs_intact(&messages);
    }

    #[test]
    fn results_of_the_newest_turn_are_not_replaced_before_the_model_read_them() {
        // One turn of six parallel calls: the model has seen none of the results yet.
        let ids: Vec<String> = (0..6).map(|n| format!("c{n}")).collect();
        let mut messages = vec![Message::system("s"), Message::user("q")];
        messages.push(Message {
            tool_calls: ids
                .iter()
                .map(|id| ToolCall { id: id.clone(), name: "t".into(), arguments: "{}".into() })
                .collect(),
            ..Message::assistant("")
        });
        messages.extend(ids.iter().map(|id| Message::tool(id.clone(), format!("start {id} {}", "r".repeat(7_000)))));
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size / 2, size / 2, Keep { tool_results: 4, turns: 2 }, Scope::default());
        assert_eq!(done.placeholders, 0);
        assert!(done.cut > 0, "they are cut instead, and every one keeps its start");
        for (message, id) in messages.iter().skip(3).zip(&ids) {
            assert!(message.text().starts_with(&format!("start {id} ")), "{}", message.text());
        }
        assert!(done.after <= size / 2);
        // The same holds when no result is kept by count.
        let mut one = conversation(1, 7_000);
        let done = trim(&mut one, 0, 0, Keep { tool_results: 0, turns: 1 }, Scope::default());
        assert_eq!(done.placeholders, 0);
        assert!(one.last().is_some_and(|result| result.text().starts_with("xxxx")));
    }

    #[test]
    fn a_result_that_imitates_a_placeholder_is_still_cut() {
        let fake = format!("{PLACEHOLDER_PREFIX}: 1 characters] {}", "x".repeat(100_000));
        let mut messages = vec![Message::system("s"), Message::user("q"), call("c0"), Message::tool("c0", fake)];
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size / 2, size / 2, KEEP, Scope::default());
        assert_eq!(done.cut, 1);
        assert!(done.after <= size / 2);
    }

    #[test]
    fn pinned_results_and_the_tail_are_never_touched() {
        let mut messages = conversation(6, 7_000);
        let before = messages.clone();
        let text = before[5].text();
        // The id alone would not do: a result with the same id and another text is not pinned.
        let pinned = [("c0", text.as_str()), ("c1", "something else")];
        // The last four messages are a tail the caller protects.
        let scope = Scope { pinned: &pinned, tail: 4 };
        let done = trim(&mut messages, 0, 0, Keep { tool_results: 0, turns: 1 }, scope);
        // The pinned result and its call stay as they were, although they are the oldest.
        let at = messages.iter().position(|m| m.tool_call_id.as_deref() == Some("c0"));
        let at = at.unwrap_or_default();
        assert!(messages[at] == before[5] && messages[at - 1] == before[4], "the pinned result or its call changed");
        // So does the tail, apart from the cut of the last resort, which keeps every message.
        let tail: Vec<_> = messages[messages.len() - 4..].iter().map(|m| m.tool_call_id.clone()).collect();
        assert_eq!(tail, [None, Some("c4".to_owned()), None, Some("c5".to_owned())]);
        assert!(done.removed.iter().all(|m| m.tool_call_id.as_deref() != Some("c0")));
        assert_eq!(done.removed.len(), 2 + 6, "the earlier turn and three tool turns");
        assert_pairs_intact(&messages);
    }

    #[test]
    fn call_ids_that_repeat_from_turn_to_turn_do_not_confuse_the_trim() {
        // Some providers number the calls of each response from zero.
        let mut messages = vec![Message::system("s"), Message::user("q")];
        for turn in 0..6 {
            messages.push(call("call_0"));
            messages.push(Message::tool("call_0", format!("turn {turn} {}", "r".repeat(7_000))));
        }
        let first = messages[3].text();
        let pinned = [("call_0", first.as_str())];
        let size = estimate_messages(&messages);
        // Placeholders only: the first result is pinned, the next three are replaced.
        let done = trim(&mut messages, size * 6 / 10, size - 1, KEEP, Scope { pinned: &pinned, tail: 0 });
        assert_eq!(messages[3].text(), first, "only the one pinned result is held");
        assert_eq!(done.placeholders, 3);
        let replaced: Vec<String> = done.replaced.iter().map(|m| m.text().chars().take(6).collect()).collect();
        assert_eq!(replaced, ["turn 1", "turn 2", "turn 3"]);
        // A later trim removes groups that already hold placeholders: each original comes back once.
        let done = trim(&mut messages, 3_000, 3_000, KEEP, Scope { pinned: &pinned, tail: 0 });
        assert_eq!(done.removed.len(), 6);
        assert!(
            done.removed.iter().filter(|m| m.role == Role::Tool).all(is_placeholder),
            "placeholders of the earlier trim"
        );
        assert_eq!(done.after, estimate_messages(&messages));
        assert_pairs_intact_by_position(&messages);
    }

    #[test]
    fn removed_messages_come_back_oldest_first_whatever_their_kind() {
        let mut messages = vec![Message::system("s")];
        for turn in 0..4 {
            messages.push(Message::user(format!("question {turn}")));
            messages.push(call(&format!("c{turn}")));
            messages.push(Message::tool(format!("c{turn}"), "r".repeat(3_000)));
            messages.push(Message::assistant(format!("answer {turn}")));
        }
        messages.push(Message::user("the request"));
        let done = trim(&mut messages, 0, 0, Keep { tool_results: 0, turns: 1 }, Scope::default());
        let order: Vec<String> = done
            .removed
            .iter()
            .filter(|m| m.role == Role::User || !m.tool_calls.is_empty())
            .map(|m| if m.role == Role::User { m.text() } else { m.tool_calls[0].id.clone() })
            .collect();
        assert_eq!(order, ["question 0", "c0", "question 1", "c1", "question 2", "c2", "question 3", "c3"]);
    }

    #[test]
    fn a_call_without_all_its_results_yet_survives() {
        // A turn whose second call still waits for approval.
        let mut messages = conversation(3, 7_000);
        messages.push(Message {
            tool_calls: ["w0", "w1"]
                .iter()
                .map(|id| ToolCall { id: (*id).into(), name: "t".into(), arguments: "{}".into() })
                .collect(),
            ..Message::assistant("")
        });
        messages.push(Message::tool("w0", "r".repeat(7_000)));
        trim(&mut messages, 0, 0, Keep { tool_results: 0, turns: 1 }, Scope::default());
        let last_call = messages.iter().rfind(|m| !m.tool_calls.is_empty()).unwrap();
        assert_eq!(last_call.tool_calls.iter().map(|call| call.id.as_str()).collect::<Vec<_>>(), ["w0", "w1"]);
        assert_eq!(messages.last().unwrap().tool_call_id.as_deref(), Some("w0"));
    }

    #[test]
    fn the_last_resort_drops_images_and_says_so() {
        let mut result = Message::tool_with_images(
            "c0",
            "日".repeat(40_000),
            ["data:image/png;base64,AAAA".to_owned(), "data:image/png;base64,BBBB".to_owned()],
        );
        result.tool_call_id = Some("c0".into());
        let mut messages = vec![Message::system("s"), Message::user("q"), call("c0"), result];
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size / 4, size / 4, KEEP, Scope::default());
        assert!(done.cut >= 1);
        assert_eq!(messages[3].images().count(), 0);
        assert!(messages[3].text().ends_with("[2 image(s) removed to fit the context window]"));
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c0"));
        assert_eq!(done.after, estimate_messages(&messages));
    }

    #[test]
    fn the_estimate_of_text_matches_a_count_per_character() {
        let samples = [
            "",
            "a",
            "abcdefg",
            "abcdefgh",
            "abcdefghi",
            "aé",
            "日本語abc",
            "1234567é",
            "😀1234567890é日",
            "ééééééééé",
        ];
        for sample in samples {
            for repeat in [1, 3, 17] {
                let text = sample.repeat(repeat);
                let ascii = text.chars().filter(char::is_ascii).count() as u64;
                let other = text.chars().filter(|c| !c.is_ascii()).count() as u64;
                assert_eq!(text_tokens(&text), (ascii * 2).div_ceil(7) + other, "{text:?}");
            }
        }
    }

    #[test]
    fn system_messages_the_request_and_the_newest_turns_stay() {
        let mut messages = conversation(3, 7_000);
        let done = trim(&mut messages, 0, 0, Keep { tool_results: 0, turns: 1 }, Scope::default());
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User, Role::Assistant, Role::Tool]);
        assert_eq!(messages[0].text(), "You are helpful.");
        assert_eq!(messages[1].text(), "the request");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c2"));
        assert!(done.after > 0, "what is protected cannot be removed");
        assert_pairs_intact(&messages);
    }

    #[test]
    fn at_least_one_turn_is_kept_whatever_the_setting() {
        let mut messages = conversation(2, 100);
        trim(&mut messages, 0, 0, Keep { tool_results: 0, turns: 0 }, Scope::default());
        assert_eq!(messages.last().unwrap().tool_call_id.as_deref(), Some("c1"));
        assert_pairs_intact(&messages);
    }

    #[test]
    fn parallel_calls_stay_with_all_their_results() {
        let mut messages = vec![Message::system("s"), Message::user("q")];
        for turn in 0..4 {
            let ids = [format!("a{turn}"), format!("b{turn}")];
            messages.push(Message {
                tool_calls: ids
                    .iter()
                    .map(|id| ToolCall { id: id.clone(), name: "t".into(), arguments: "{}".into() })
                    .collect(),
                ..Message::assistant("thinking")
            });
            for id in ids {
                messages.push(Message::tool(id, "r".repeat(3_000)));
            }
        }
        let done = trim(&mut messages, 1_000, 1_000, Keep { tool_results: 2, turns: 1 }, Scope::default());
        assert_eq!(done.removed.len() % 3, 0, "groups of one call message and two results");
        assert!(!done.removed.is_empty());
        assert_pairs_intact(&messages);
    }

    /// Generated conversations of many shapes: after any trim, every tool
    /// result still has its call and every call its results, system messages
    /// and the last user message are still there, and the estimate is right.
    #[test]
    fn pairs_survive_every_trim() {
        // A small deterministic generator: no dependency for one test.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for case in 0..300 {
            let mut messages = vec![Message::system("s")];
            let mut calls = 0;
            for _ in 0..next(4) {
                messages.push(Message::user("h".repeat(next(400) as usize)));
                messages.push(Message::assistant("a".repeat(next(400) as usize)));
            }
            messages.push(Message::user("the request"));
            for _ in 0..next(12) {
                let parallel = 1 + next(3);
                let ids: Vec<String> = (0..parallel).map(|n| format!("c{calls}-{n}")).collect();
                calls += 1;
                messages.push(Message {
                    tool_calls: ids
                        .iter()
                        .map(|id| ToolCall { id: id.clone(), name: "t".into(), arguments: "{}".into() })
                        .collect(),
                    ..Message::assistant("")
                });
                for id in ids {
                    messages.push(Message::tool(id, "r".repeat(next(6_000) as usize)));
                }
            }
            let size = estimate_messages(&messages);
            let limit = next(size + 1);
            let keep = Keep { tool_results: next(4) as usize, turns: next(4) as usize };
            let done = trim(&mut messages, limit * 3 / 4, limit, keep, Scope::default());
            assert_pairs_intact(&messages);
            assert_eq!(messages[0].role, Role::System, "case {case}");
            assert!(messages.iter().any(|m| m.text() == "the request"), "case {case}");
            assert_eq!(done.after, estimate_messages(&messages), "case {case}");
            assert!(done.after <= done.before);
        }
    }

    #[test]
    fn newest_turns_pending_calls_and_pinned_skills_survive_every_trim() {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for case in 0..300 {
            let mut messages = vec![Message::system("s"), Message::user("the request")];
            let mut skills: Vec<(String, String)> = Vec::new();
            let turns = 1 + next(10);
            for turn in 0..turns {
                // Ids repeat from turn to turn, as some providers number them.
                let parallel = 1 + next(3);
                let ids: Vec<String> = (0..parallel).map(|n| format!("call_{n}")).collect();
                messages.push(Message {
                    tool_calls: ids
                        .iter()
                        .map(|id| ToolCall { id: id.clone(), name: "t".into(), arguments: "{}".into() })
                        .collect(),
                    ..Message::assistant("")
                });
                // The newest call sometimes waits for approval: no result yet.
                if turn + 1 == turns && next(3) == 0 {
                    break;
                }
                for id in ids {
                    if next(4) == 0 {
                        let text = format!("SKILL {case}-{turn}-{id} {}", "k".repeat(next(3_000) as usize));
                        messages.push(Message::tool(id.clone(), text.clone()));
                        skills.push((id, text));
                    } else {
                        messages.push(Message::tool(id, "r".repeat(next(6_000) as usize)));
                    }
                }
            }
            let before = messages.clone();
            let newest = before.iter().rposition(|m| !m.tool_calls.is_empty()).unwrap();
            let pinned: Vec<(&str, &str)> = skills.iter().map(|(id, text)| (id.as_str(), text.as_str())).collect();
            let size = estimate_messages(&messages);
            let limit = next(size + 1);
            let keep = Keep { tool_results: next(4) as usize, turns: next(4) as usize };
            trim(&mut messages, limit * 3 / 4, limit, keep, Scope { pinned: &pinned, tail: 0 });
            // The newest model turn is there with its calls, and its results are not placeholders.
            let at = messages.iter().rposition(|m| !m.tool_calls.is_empty()).unwrap();
            let pending = newest + 1 == before.len();
            assert_pairs_intact_by_position(if pending { &messages[..at] } else { &messages });
            assert_eq!(messages[at].tool_calls, before[newest].tool_calls, "case {case}");
            assert_eq!(messages.len() - at, before.len() - newest, "case {case}");
            for result in &messages[at + 1..] {
                assert!(!result.text().starts_with(PLACEHOLDER_PREFIX), "case {case}");
            }
            // Every loaded skill is still there unchanged.
            for (id, text) in &skills {
                assert!(
                    messages.iter().any(|m| m.tool_call_id.as_deref() == Some(id.as_str()) && m.text() == *text),
                    "case {case}: skill {text:.20} was trimmed"
                );
            }
        }
    }

    #[test]
    fn the_last_resort_cuts_the_largest_result_in_the_middle() {
        let mut messages = vec![Message::system("s"), Message::user("q"), call("c0")];
        let result = format!("HEAD{}TAIL", "m".repeat(40_000));
        messages.push(Message::tool("c0", result));
        let size = estimate_messages(&messages);
        let done = trim(&mut messages, size / 2, size / 2, Keep { tool_results: 4, turns: 2 }, Scope::default());
        assert_eq!((done.placeholders, done.removed.len(), done.cut), (0, 0, 1));
        let text = messages[3].text();
        assert!(text.starts_with("HEAD") && text.ends_with("TAIL"));
        assert!(text.contains("characters removed to fit the context window]"));
        assert!(done.after <= size / 2, "{} > {}", done.after, size / 2);
        assert_eq!(done.after, estimate_messages(&messages));
        assert_pairs_intact(&messages);
    }

    #[test]
    fn what_cannot_be_trimmed_is_reported_as_still_too_large() {
        let mut messages = vec![Message::system("s".repeat(50_000)), Message::user("q")];
        let done = trim(&mut messages, 100, 100, KEEP, Scope::default());
        assert!(!done.changed());
        assert!(done.after > 100);
    }

    #[test]
    fn cutting_respects_character_boundaries() {
        let text = "日本語".repeat(500);
        let cut = cut_middle(&text, 1_500, 400);
        assert!(cut.starts_with("日本語") && cut.ends_with("日本語"));
        assert!(cut.contains("[1100 characters removed"));
        assert_eq!(cut_middle("short", 5, 10), "short");
    }

    #[test]
    fn a_stray_tool_result_is_never_left_alone() {
        let mut messages = vec![
            Message::system("s"),
            Message::user("old"),
            Message::assistant("old answer"),
            Message::tool("ghost", "g".repeat(2_000)),
            Message::user("q"),
            call("c0"),
            Message::tool("c0", "r".repeat(2_000)),
        ];
        let done = trim(&mut messages, 100, 100, Keep { tool_results: 1, turns: 1 }, Scope::default());
        // The stray result left with the group before it, and comes back as it was.
        assert_eq!(done.removed.len(), 3);
        assert_eq!(done.removed[2].text().len(), 2_000);
        assert!(messages.iter().all(|m| m.tool_call_id.as_deref() != Some("ghost")));
    }
}
