//! `diagram`: a diagram written as [Mermaid](https://mermaid.js.org) text.
//!
//! ```json
//! {"$ui": "diagram", "version": 1, "title": "Order flow", "syntax": "mermaid",
//!  "source": "flowchart TD\n  A[Customer asks] --> B{In stock?}\n  B -- Yes --> C[Take order]\n  B -- No --> D[Offer another]"}
//! ```
//!
//! - `syntax`: `mermaid` (the only one so far).
//! - `source`: the diagram text, at most 20 000 characters and 500 lines. Its first
//!   statement names the diagram type, one of [`DIAGRAM_TYPES`].
//!
//! The source is text only, so hosts can draw it in their own theme without running
//! anything it brings along. Rejected: HTML tags (`<br>` is the one exception, a line
//! break in a label), `click` / `link` / `callback` statements and `href`, config in
//! front matter or `%%{...}%%` directives, `style` / `classDef` / `linkStyle` colours,
//! images in nodes, and `javascript:` / `data:` URLs.
//!
//! Hosts still render with scripts and HTML labels switched off (Mermaid's
//! `securityLevel: "strict"`, `htmlLabels: false`): this check keeps the tool error
//! helpful and the block small, it is not the only guard.

use crate::UiError;
use crate::checks::{Fields, MAX_LONG_TEXT, MAX_SHORT_TEXT, only_known, optional_text, required_text};

/// Diagram syntaxes accepted in `syntax`.
pub const SYNTAXES: &[&str] = &["mermaid"];
/// Mermaid diagram types a `source` may start with. Each draws without HTML labels.
pub const DIAGRAM_TYPES: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "stateDiagram-v2",
    "erDiagram",
    "gantt",
    "pie",
    "mindmap",
    "timeline",
    "quadrantChart",
    "gitGraph",
];
/// Longest `source`, in characters.
pub const MAX_SOURCE_CHARS: usize = 20_000;
/// Most lines in `source`.
pub const MAX_SOURCE_LINES: usize = 500;

const FIELDS: &[&str] = &["title", "subtitle", "fallback_text", "syntax", "source"];
/// Statements that attach a link or a script call to a node, by their first word.
const INTERACTION_KEYWORDS: &[&str] = &["click", "callback", "link", "links"];
/// Diagram types where a line starting with an [`INTERACTION_KEYWORDS`] word is a statement
/// (elsewhere, such as a mind map, it is ordinary label text).
const TYPES_WITH_INTERACTION: &[&str] =
    &["flowchart", "graph", "sequenceDiagram", "classDiagram", "stateDiagram", "stateDiagram-v2", "gantt"];
/// Statements that set colours, by their first word.
const STYLE_KEYWORDS: &[&str] = &["style", "classDef", "linkStyle"];
/// Diagram types where a line starting with a [`STYLE_KEYWORDS`] word is a statement.
const TYPES_WITH_STYLE: &[&str] =
    &["flowchart", "graph", "classDiagram", "stateDiagram", "stateDiagram-v2", "erDiagram", "quadrantChart"];
const URL_SCHEMES: &[&str] = &["javascript:", "vbscript:", "data:"];

pub(crate) fn validate(fields: &Fields) -> Result<(), UiError> {
    only_known(fields, "", FIELDS)?;
    optional_text(fields, "title", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "subtitle", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "fallback_text", "", MAX_LONG_TEXT)?;
    let syntax = required_text(fields, "syntax", "", MAX_SHORT_TEXT)?;
    if !SYNTAXES.contains(&syntax) {
        return Err(UiError::new("/syntax", format!("`{syntax}` is not one of: {}", SYNTAXES.join(", "))));
    }
    let source = required_text(fields, "source", "", MAX_SOURCE_CHARS)?;
    check_source(source).map(|_| ()).map_err(|message| UiError::new("/source", message))
}

/// The diagram type `source` starts with (`flowchart`, `sequenceDiagram`...), when it is
/// one of [`DIAGRAM_TYPES`]. Blank lines and `%%` comments before it are skipped.
///
/// ```
/// assert_eq!(graphrt_ui::diagram::diagram_type("%% orders\nflowchart LR\n  A --> B"), Some("flowchart"));
/// assert_eq!(graphrt_ui::diagram::diagram_type("A --> B"), None);
/// ```
pub fn diagram_type(source: &str) -> Option<&'static str> {
    let first = source.lines().map(str::trim).find(|line| !line.is_empty() && !line.starts_with("%%"))?;
    let word = first.split(|c: char| c.is_whitespace() || c == ';').next().unwrap_or("");
    DIAGRAM_TYPES.iter().copied().find(|known| *known == word)
}

/// Checks Mermaid `source` and returns its diagram type; the error says what to change.
fn check_source(source: &str) -> Result<&'static str, String> {
    if source.trim().is_empty() {
        return Err("must not be empty".to_owned());
    }
    if source.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err("must not contain control characters".to_owned());
    }
    let lines = source.lines().count();
    if lines > MAX_SOURCE_LINES {
        return Err(format!("has {lines} lines; at most {MAX_SOURCE_LINES} are allowed"));
    }
    if source.trim_start().starts_with("---") {
        return Err("front matter (`---`) is not supported: start with the diagram type and put the title in the block's `title`".to_owned());
    }
    if source.contains("%%{") {
        return Err("directives (`%%{...}%%`) are not supported: the host chooses theme and settings".to_owned());
    }
    let Some(kind) = diagram_type(source) else {
        return Err(format!("must start with a diagram type, one of: {}", DIAGRAM_TYPES.join(", ")));
    };
    let lower = source.to_ascii_lowercase();
    if let Some(scheme) = URL_SCHEMES.iter().find(|scheme| lower.contains(*scheme)) {
        return Err(format!("`{scheme}` URLs are not allowed in a diagram"));
    }
    for (number, line) in source.lines().enumerate() {
        check_line(line, kind).map_err(|message| format!("line {}: {message}", number + 1))?;
    }
    Ok(kind)
}

fn check_line(line: &str, kind: &str) -> Result<(), String> {
    let statement = line.trim();
    if statement.starts_with("%%") {
        return Ok(());
    }
    let first_word = statement.split(char::is_whitespace).next().unwrap_or("");
    let has_argument = statement.len() > first_word.len();
    if has_argument && TYPES_WITH_INTERACTION.contains(&kind) && INTERACTION_KEYWORDS.contains(&first_word) {
        return Err(format!(
            "`{first_word}` statements (links and callbacks) are not supported: a diagram is a picture, remove this line"
        ));
    }
    if has_argument && TYPES_WITH_STYLE.contains(&kind) && STYLE_KEYWORDS.contains(&first_word) {
        return Err(format!(
            "`{first_word}` is not supported: the host colours the diagram in its own theme, remove this line"
        ));
    }
    if let Some(tag) = html_tag(statement) {
        return Err(format!("HTML (`{tag}`) is not allowed: write plain text, and `<br>` for a line break in a label"));
    }
    if let Some(shape) = statement.find("@{").and_then(|start| statement.get(start..)) {
        let compact: String = shape.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.contains("img:") {
            return Err("images in nodes are not supported".to_owned());
        }
    }
    Ok(())
}

/// The first HTML tag in `line` other than `<br>`, e.g. `<img src=x>`. Arrows (`-->`,
/// `<|--`, `->>`), stereotypes (`<<interface>>`) and a lone `<` are not tags.
fn html_tag(line: &str) -> Option<&str> {
    let mut previous = None;
    for (start, c) in line.char_indices() {
        let after_angle = previous == Some('<');
        previous = Some(c);
        if c != '<' || after_angle {
            continue;
        }
        let rest = line.get(start + 1..).unwrap_or("");
        let name_start = rest.strip_prefix('/').unwrap_or(rest);
        if !name_start.chars().next().is_some_and(|first| first.is_ascii_alphabetic()) {
            continue;
        }
        let Some(end) = rest.find(['<', '>']).filter(|end| rest.get(*end..).is_some_and(|tail| tail.starts_with('>')))
        else {
            continue;
        };
        let inner = rest.get(..end).unwrap_or("");
        let name: String = name_start.chars().take_while(char::is_ascii_alphanumeric).collect();
        let is_line_break = name.eq_ignore_ascii_case("br") && inner.trim_end_matches('/').trim().len() == name.len();
        if !is_line_break {
            return line.get(start..start + end + 2);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;
    use serde_json::json;

    fn error(source: &str) -> String {
        validate(&json!({"$ui": "diagram", "version": 1, "syntax": "mermaid", "source": source}))
            .unwrap_err()
            .to_string()
    }

    fn accepted(source: &str) -> bool {
        validate(&json!({"$ui": "diagram", "version": 1, "syntax": "mermaid", "source": source})).is_ok()
    }

    #[test]
    fn accepts_the_common_diagram_types() {
        assert!(accepted("flowchart TD\n  A[Ask] --> B{In stock?}\n  B -- Yes --> C[Order]\n  A <--> C"));
        assert!(accepted("%% a comment first\n\ngraph LR;\n  A[\"One<br>two<br/>three<BR />four\"] --> B"));
        assert!(accepted("sequenceDiagram\n  Customer->>Bot: Hi\n  Bot-->>Customer: Hello\n  Bot-)Customer: async"));
        assert!(accepted(
            "classDiagram\n  class Animal {\n    <<interface>>\n    +eat() void\n  }\n  Animal <|-- Duck"
        ));
        assert!(accepted("stateDiagram-v2\n  [*] --> Draft\n  Draft --> [*]: done"));
        assert!(accepted("erDiagram\n  CUSTOMER ||--o{ ORDER : places\n  ORDER }|..|{ ITEM : has"));
        assert!(accepted("pie title Channels\n  \"WhatsApp\" : 60\n  \"Email\" : 40"));
        assert!(accepted("mindmap\n  root((Sales))\n    link building\n    click rate\n    style guide"));
        assert!(accepted("flowchart LR\n  A[price < 5 and stock > 2] --> B[a<b]"));
    }

    #[test]
    fn the_source_must_name_a_known_diagram_type() {
        assert!(error("A --> B").contains("must start with a diagram type"));
        assert!(error("journey\n  title x").contains("must start with a diagram type"));
        assert!(error("   \n").contains("must not be empty"));
        assert_eq!(diagram_type("stateDiagram-v2\n [*] --> A"), Some("stateDiagram-v2"));
        assert_eq!(diagram_type("pie title x"), Some("pie"));
    }

    #[test]
    fn rejects_html_links_callbacks_and_config() {
        assert!(
            error("flowchart TD\n  A[\"<img src=x onerror=alert(1)>\"] --> B")
                .contains("line 2: HTML (`<img src=x onerror=alert(1)>`)")
        );
        assert!(error("flowchart TD\n  A --> B[<b>bold</b>]").contains("HTML (`<b>`)"));
        assert!(
            error("flowchart TD\n  A --> B\n  click A href \"https://example.com\"")
                .contains("line 3: `click` statements")
        );
        assert!(error("flowchart TD\n  A --> B\n  click A call run()").contains("`click` statements"));
        assert!(
            error("sequenceDiagram\n  A->>B: hi\n  link A: Docs @ https://example.com").contains("`link` statements")
        );
        assert!(error("classDiagram\n  class A\n  callback A \"run\"").contains("`callback` statements"));
        assert!(error("---\nconfig:\n  securityLevel: loose\n---\nflowchart TD\n A-->B").contains("front matter"));
        assert!(error("%%{init: {'securityLevel': 'loose'}}%%\nflowchart TD\n A-->B").contains("directives"));
        assert!(error("flowchart TD\n  A --> B %%{init: {}}%%").contains("directives"));
        assert!(error("flowchart TD\n  A[\"JAVASCRIPT:alert(1)\"] --> B").contains("`javascript:` URLs"));
        assert!(error("flowchart TD\n  A@{ img: \"https://example.com/a.png\" } --> B").contains("images in nodes"));
    }

    #[test]
    fn rejects_colours_because_the_host_themes_the_diagram() {
        assert!(error("flowchart TD\n  A --> B\n  style A fill:#f9f").contains("line 3: `style` is not supported"));
        assert!(
            error("flowchart TD\n  classDef red fill:#f00\n  A:::red --> B").contains("`classDef` is not supported")
        );
        assert!(error("graph LR\n  A --> B\n  linkStyle 0 stroke:#f00").contains("`linkStyle` is not supported"));
    }

    #[test]
    fn limits_size_and_requires_the_syntax() {
        let long_line = format!("flowchart TD\n  A[{}] --> B", "x".repeat(MAX_SOURCE_CHARS));
        assert!(error(&long_line).contains("characters long"));
        let many_lines = format!("flowchart TD\n{}", "  A --> B\n".repeat(MAX_SOURCE_LINES));
        assert!(error(&many_lines).contains("lines; at most 500"));
        assert!(error("flowchart TD\n  A --> B\u{0007}").contains("control characters"));
        let no_syntax = validate(&json!({"$ui": "diagram", "version": 1, "source": "flowchart TD\n A-->B"}));
        assert!(no_syntax.unwrap_err().to_string().contains("`syntax` is required"));
        let other = validate(&json!({"$ui": "diagram", "version": 1, "syntax": "plantuml", "source": "x"}));
        assert!(other.unwrap_err().to_string().contains("is not one of: mermaid"));
        let extra = validate(
            &json!({"$ui": "diagram", "version": 1, "syntax": "mermaid", "source": "pie\n \"a\": 1", "theme": "dark"}),
        );
        assert!(extra.unwrap_err().to_string().contains("unknown field `theme`"));
    }
}
