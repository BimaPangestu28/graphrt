//! Speed of the document writers, for the numbers in docs/files-and-images-design.md,
//! and sample files to open in a reader.
//!
//! ```sh
//! cargo run --release -p graphrt-docgen --example bench            # timings
//! cargo run --release -p graphrt-docgen --example bench -- ./out   # also write samples there
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss, clippy::indexing_slicing)]

use std::time::Instant;

use graphrt_docgen::xlsx::{Cell, Sheet, XlsxLimits};
use graphrt_docgen::{docx, markdown, pdf, xlsx};

const REPORT: &str = "# Quarterly sales report\n\nRevenue grew **12%** over the quarter, driven by *Jakarta* and Surabaya. \
The caf\u{e9} segment \u{2013} new this year \u{2013} reached 5 \u{20ac} per order. Use `files/read` to see the raw numbers.\n\n\
## By region\n\n| Region | Revenue | Change | Notes |\n|---|---:|---:|---|\n| Jakarta | 120.5 | +14% | Two new outlets opened in the south of the city during August |\n\
| Surabaya | 80.0 | +9% | Stable |\n| Medan | 41.2 | -2% | Supply delays |\n\n## Next steps\n\n1. Open a third outlet in Jakarta\n2. Fix the supply chain in Medan,\n   starting with the cold storage contract\n3. Review prices\n\n\
- Keep the weekly report\n- Share this document with the regional leads\n\n---\n\n### Method\n\n```\nrevenue = orders * average_price\n  change = (this - last) / last\n```\n";

fn median_ms(runs: usize, mut work: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let started = Instant::now();
            work();
            started.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    let long = REPORT.repeat(40);
    let rows: Vec<Vec<Cell>> = (0..10_000)
        .map(|row| {
            vec![
                Cell::Number(f64::from(row)),
                Cell::Text(format!("Customer {row}")),
                Cell::Number(f64::from(row) * 1.5),
                Cell::Bool(row % 2 == 0),
                Cell::Text("=SUM(A1:A9)".into()),
            ]
        })
        .collect();
    let sheets = vec![Sheet { name: "Orders".into(), rows, header: true }];

    println!("| What | Result |\n| --- | --- |");
    let ms = median_ms(200, || {
        std::hint::black_box(markdown::parse(std::hint::black_box(&long)));
    });
    println!("| Read {} KiB of Markdown | {ms:.2} ms |", long.len() / 1024);
    let blocks = markdown::parse(&long);
    let one_page = markdown::parse(REPORT);
    let ms = median_ms(500, || {
        std::hint::black_box(pdf::render(&one_page, &pdf::PdfOptions::default()).unwrap());
    });
    println!("| PDF, a one page report | {ms:.3} ms |");
    let document = pdf::render(&blocks, &pdf::PdfOptions::default()).unwrap();
    let ms = median_ms(50, || {
        std::hint::black_box(pdf::render(&blocks, &pdf::PdfOptions::default()).unwrap());
    });
    println!("| PDF, {} pages | {ms:.1} ms, {} KiB |", document.pages, document.bytes.len() / 1024);
    let word = docx::render(&blocks, &docx::DocxOptions::default()).unwrap();
    let ms = median_ms(50, || {
        std::hint::black_box(docx::render(&blocks, &docx::DocxOptions::default()).unwrap());
    });
    println!("| DOCX, the same content | {ms:.1} ms, {} KiB |", word.len() / 1024);
    let book = xlsx::render(&sheets, &XlsxLimits::default()).unwrap();
    let ms = median_ms(20, || {
        std::hint::black_box(xlsx::render(&sheets, &XlsxLimits::default()).unwrap());
    });
    println!("| XLSX, 10,000 rows of 5 cells | {ms:.1} ms, {} KiB |", book.len() / 1024);

    if let Some(dir) = std::env::args().nth(1) {
        std::fs::create_dir_all(&dir).unwrap();
        let sample =
            pdf::render(&one_page, &pdf::PdfOptions { title: Some("Sales report".into()), ..Default::default() });
        std::fs::write(format!("{dir}/report.pdf"), sample.unwrap().bytes).unwrap();
        std::fs::write(format!("{dir}/long.pdf"), document.bytes).unwrap();
        let word = docx::render(&one_page, &docx::DocxOptions { title: Some("Sales report".into()) }).unwrap();
        std::fs::write(format!("{dir}/report.docx"), word).unwrap();
        std::fs::write(format!("{dir}/orders.xlsx"), book).unwrap();
    }
}
