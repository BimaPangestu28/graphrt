//! Behaviour of builds without optional format features.

#![cfg(not(any(feature = "pdf", feature = "office", feature = "spreadsheet", feature = "epub", feature = "email")))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use graphrt_doc::{Options, extract};

#[test]
fn disabled_formats_name_their_feature() {
    for (bytes, name, feature) in [
        (&b"%PDF-1.4\n"[..], "a.pdf", "pdf"),
        (&b"PK\x03\x04"[..], "a.docx", "office"),
        (&b"PK\x03\x04"[..], "a.xlsx", "spreadsheet"),
        (&b"From: a@b.c\r\n\r\nhi"[..], "a.eml", "email"),
    ] {
        let err = extract(bytes, None, Some(name), &Options::default()).unwrap_err();
        assert_eq!(err.kind(), "unsupported_format", "{name}");
        assert!(err.to_string().contains(&format!("`{feature}` feature")), "{name}: {err}");
    }
}

#[test]
fn text_formats_always_work() {
    let html = b"<p>Hello <b>there</b></p><script>x</script>";
    assert_eq!(extract(html, Some("text/html"), None, &Options::default()).unwrap(), "Hello there");
    assert_eq!(extract(b"plain", None, Some("a.txt"), &Options::default()).unwrap(), "plain");
}
