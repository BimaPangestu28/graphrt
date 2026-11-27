//! Web connector against a mock site: crawl bounds, robots.txt, incremental
//! syncs (add, change, delete), sitemaps with lastmod, and the SSRF guard.

mod common;

use common::{Mock, Reply, all_text, build, documents, knowledge};
use graphrt_connect::{
    BuildContext, ConnectorRegistry, MemorySyncState, SourceConfig, SyncOptions, SyncOutcome, document_id, sync_source,
};
use serde_json::json;

fn page(title: &str, body: &str, links: &[&str]) -> Reply {
    let anchors: String = links.iter().map(|l| format!(r#"<a href="{l}">{l}</a> "#)).collect();
    Reply::html(&format!(
        "<html><head><title>{title}</title></head><body><h1>{title}</h1><p>{body}</p>{anchors}</body></html>"
    ))
}

#[tokio::test]
async fn crawl_is_bounded_honours_robots_and_syncs_incrementally() {
    let site = Mock::start().await;
    site.route("GET /robots.txt", Reply::text("text/plain", "User-agent: *\nDisallow: /private\n"));
    site.route(
        "GET /",
        page(
            "Home",
            "Welcome to the coffee shop.",
            &["/menu", "/about", "/private/admin", "https://elsewhere.example/"],
        ),
    );
    site.route("GET /menu", page("Menu", "Latte costs 4 euro.", &["/menu/seasonal"]));
    site.route("GET /about", page("About", "Open since 2019.", &[]));
    site.route("GET /menu/seasonal", page("Seasonal", "Pumpkin latte in autumn.", &[]));
    site.route("GET /private/admin", page("Admin", "secret", &[]));

    let source = SourceConfig::new(
        "site",
        "web",
        json!({ "urls": [format!("{}/", site.base)], "mode": "crawl", "max_depth": 1, "allow_private": true }),
    )
    .dataset("kb");
    let connector = build(&source, &|_| None);
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    let options = SyncOptions::default();

    let status = sync_source(&source, connector.as_ref(), &sink, &state, &options).await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded, "{status:?}");
    assert_eq!(status.report.indexed, 3, "home, menu and about; seasonal is two links away");
    assert_eq!(site.count("GET", "/private"), 0, "robots.txt disallows /private");
    let text = all_text(&kb).await;
    assert!(text.contains("Latte costs 4 euro") && !text.contains("Pumpkin") && !text.contains("secret"));

    // Nothing changed: nothing is indexed again.
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &options).await.unwrap();
    assert_eq!((status.report.indexed, status.report.unchanged, status.report.removed), (0, 3, 0));

    // The menu changes and the about page is no longer linked (deleted); the
    // home page changes too (one link less).
    site.route("GET /", page("Home", "Welcome to the coffee shop.", &["/menu"]));
    site.route("GET /menu", page("Menu", "Latte costs 5 euro now.", &[]));
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &options).await.unwrap();
    assert_eq!((status.report.indexed, status.report.unchanged, status.report.removed), (2, 0, 1), "{status:?}");
    let text = all_text(&kb).await;
    assert!(text.contains("5 euro now") && !text.contains("4 euro") && !text.contains("Open since 2019"));
    assert_eq!(status.document_count, 2);
    let home = document_id("site", &format!("{}/", site.base));
    assert!(documents(&kb).await.contains(&home));

    // Hits carry the source and the page address.
    let hits = kb.retrieve(&graphrt_rag::RetrievalRequest::new("latte price", vec!["kb".into()])).await.unwrap();
    let hit = hits.first().expect("a hit");
    assert_eq!(hit.metadata.get("source_id"), Some(&json!("site")));
    assert!(hit.metadata.get("url").and_then(|u| u.as_str()).is_some_and(|u| u.ends_with("/menu")));
}

#[tokio::test]
async fn an_unreachable_start_page_changes_nothing() {
    let site = Mock::start().await;
    site.route("GET /", page("Home", "Hours: 8 to 18.", &[]));
    let source =
        SourceConfig::new("site", "web", json!({ "urls": [format!("{}/", site.base)], "allow_private": true }))
            .dataset("kb");
    let connector = build(&source, &|_| None);
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(documents(&kb).await.len(), 1);

    site.route("GET /", Reply::status(404));
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Failed);
    assert_eq!(status.error.as_ref().map(|e| e.kind.as_str()), Some("not_found"), "{status:?}");
    assert_eq!(documents(&kb).await.len(), 1, "the indexed page stays searchable");
}

#[tokio::test]
async fn sitemap_lastmod_skips_unchanged_pages_without_downloading() {
    let site = Mock::start().await;
    let sitemap = |b_lastmod: &str| {
        Reply::text(
            "application/xml",
            &format!(
                "<urlset><url><loc>{0}/a</loc><lastmod>2026-11-01</lastmod></url><url><loc>{0}/b</loc><lastmod>{b_lastmod}</lastmod></url></urlset>",
                site.base
            ),
        )
    };
    site.route("GET /sitemap.xml", sitemap("2026-11-02"));
    site.route("GET /a", page("A", "Alpha page.", &[]));
    site.route("GET /b", page("B", "Beta page.", &[]));
    let source = SourceConfig::new(
        "map",
        "web",
        json!({ "urls": [format!("{}/sitemap.xml", site.base)], "mode": "sitemap", "allow_private": true }),
    )
    .dataset("kb");
    let connector = build(&source, &|_| None);
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.report.indexed, 2, "{status:?}");

    site.clear_log();
    site.route("GET /sitemap.xml", sitemap("2026-11-20"));
    site.route("GET /b", page("B", "Beta page, updated.", &[]));
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!((status.report.indexed, status.report.unchanged), (1, 1));
    assert_eq!(site.count("GET", "/a"), 0, "same lastmod: not downloaded");
    assert!(all_text(&kb).await.contains("updated"));
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let site = Mock::start().await;
    site.route("GET /", page("Intranet", "internal", &[]));
    let source = SourceConfig::new("site", "web", json!({ "urls": [format!("{}/", site.base)] })).dataset("kb");
    let connector = ConnectorRegistry::with_builtins().build(&source, &BuildContext::new(&|_| None)).unwrap();
    let (kb, sink) = knowledge();
    let status = sync_source(&source, connector.as_ref(), &sink, &MemorySyncState::new(), &SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(status.outcome, SyncOutcome::Failed);
    assert_eq!(status.error.as_ref().map(|e| e.kind.as_str()), Some("denied"), "{status:?}");
    assert!(site.requests().is_empty(), "no request reached the private address");
    assert!(documents(&kb).await.is_empty());
}
