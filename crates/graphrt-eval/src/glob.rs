//! `*` wildcard matching for test filters and tool-name patterns.

/// Whether `text` matches `pattern`, where `*` matches any run of characters.
pub fn matches(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else { return text.is_empty() };
    let Some(mut rest) = text.strip_prefix(first) else { return false };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else { return rest.is_empty() };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = rest.get(at + part.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn wildcards() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abcd"));
        assert!(matches("search*", "search_orders"));
        assert!(matches("*orders", "search_orders"));
        assert!(matches("s*_*s", "search_orders"));
        assert!(matches("*", ""));
        assert!(!matches("a*b*c", "acb"));
        assert!(!matches("ab*ba", "aba"));
    }
}
