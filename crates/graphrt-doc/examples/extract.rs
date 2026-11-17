//! Print the text of a document: `cargo run -p graphrt-doc --example extract -- report.pdf [markdown]`.

#![allow(clippy::print_stdout)]

use graphrt_doc::{Options, OutputFormat, extract_detailed};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: extract <file> [markdown]")?;
    let format = match args.next().as_deref() {
        Some("markdown") => OutputFormat::Markdown,
        _ => OutputFormat::Text,
    };
    let bytes = std::fs::read(&path)?;
    let out = extract_detailed(&bytes, None, Some(&path), &Options::default().with_format(format))?;
    println!("{}", out.text);
    eprintln!("[{} | {} chars{}]", out.format, out.text.chars().count(), if out.truncated { " | truncated" } else { "" });
    Ok(())
}
