//! API, Google Drive, Notion and SharePoint connectors against mock services:
//! pagination, credentials (OAuth refresh after a 401), exports, nested
//! content, and incremental syncs.

mod common;

use std::sync::{Arc, Mutex};

use common::{Mock, Reply, all_text, build, documents, knowledge};
use graphrt_connect::{
    AuthConfig, BuildContext, ConnectorRegistry, MemorySyncState, SourceConfig, SyncOptions, SyncOutcome, sync_source,
};
use serde_json::json;

#[tokio::test]
async fn api_with_cursor_pagination_jsonpath_and_updates() {
    let api = Mock::start().await;
    let first = json!({ "data": [
        { "id": 1, "title": "Returns", "body": "Return items within 30 days.", "updated": "2026-11-01" },
        { "id": 2, "title": "Shipping", "body": "Shipping takes 2 days.", "updated": "2026-11-01" }
    ], "meta": { "next": "c2" } });
    let second = json!({ "data": [
        { "id": 3, "title": "Warranty", "body": "Two years warranty.", "updated": "2026-11-01" }
    ], "meta": { "next": null } });
    api.route("GET /articles?lang=en", Reply::json(first.clone()));
    api.route("GET /articles?lang=en&cursor=c2", Reply::json(second));
    let source = SourceConfig::new(
        "faq",
        "api",
        json!({
            "url": format!("{}/articles", api.base),
            "query": { "lang": "en" },
            "items_path": "$.data[*]",
            "title_path": "$.title",
            "text_paths": ["$.title", "$.body"],
            "updated_path": "$.updated",
            "pagination": { "type": "cursor", "param": "cursor", "cursor_path": "$.meta.next" },
            "allow_private": true
        }),
    )
    .dataset("kb")
    .auth(AuthConfig::Header { name: "X-Api-Key".into(), value_env: "FAQ_KEY".into() });
    let secrets = |name: &str| (name == "FAQ_KEY").then(|| "k-123".to_owned());
    let connector = build(&source, &secrets);
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded, "{status:?}");
    assert_eq!(status.report.indexed, 3);
    assert!(api.requests().iter().all(|r| r.headers.get("x-api-key").map(String::as_str) == Some("k-123")));
    assert!(all_text(&kb).await.contains("Two years warranty"));

    // Record 2 changes, record 3 disappears.
    let mut changed = first;
    changed["data"][1]["body"] = json!("Shipping takes 1 day.");
    changed["data"][1]["updated"] = json!("2026-11-20");
    changed["meta"]["next"] = json!(null);
    api.route("GET /articles?lang=en", Reply::json(changed));
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!((status.report.indexed, status.report.unchanged, status.report.removed), (1, 1, 1), "{status:?}");
    let text = all_text(&kb).await;
    assert!(text.contains("1 day") && !text.contains("2 days") && !text.contains("warranty"));
}

#[tokio::test]
async fn api_page_limit_never_deletes() {
    let api = Mock::start().await;
    api.route("GET /items?page=1", Reply::json(json!([{ "id": "a", "text": "Alpha" }])));
    api.route("GET /items?page=2", Reply::json(json!([{ "id": "b", "text": "Beta" }])));
    api.route("GET /items?page=3", Reply::json(json!([])));
    let options = |max_pages: u64| {
        json!({ "url": format!("{}/items", api.base), "text_paths": ["$.text"], "max_pages": max_pages,
                "pagination": { "type": "page" }, "allow_private": true })
    };
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    let full = SourceConfig::new("items", "api", options(10)).dataset("kb");
    let status =
        sync_source(&full, build(&full, &|_| None).as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.report.indexed, 2);
    let cut = SourceConfig::new("items", "api", options(1)).dataset("kb");
    let status =
        sync_source(&cut, build(&cut, &|_| None).as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.report.removed, 0, "a listing cut short removes nothing");
    assert_eq!(status.outcome, SyncOutcome::PartlySucceeded);
    assert_eq!(documents(&kb).await.len(), 2);
}

#[tokio::test]
async fn google_drive_folders_exports_and_oauth_refresh() {
    let drive = Mock::start().await;
    // Token endpoint: a new access token, and a rotated refresh token.
    drive.route(
        "POST /token",
        Reply::json(json!({ "access_token": "fresh-access", "expires_in": 3600, "refresh_token": "rotated-refresh" })),
    );
    drive.route_sequence(
        "GET /drive/v3/files",
        vec![
            // The first call is refused (an expired token); the connector refreshes and retries.
            Reply::status(401),
            Reply::json(json!({ "files": [
                { "id": "doc1", "name": "Handbook", "mimeType": "application/vnd.google-apps.document", "version": "7", "webViewLink": "https://docs.google.com/d/doc1" },
                { "id": "sub", "name": "Policies", "mimeType": "application/vnd.google-apps.folder" },
                { "id": "img", "name": "logo.png", "mimeType": "image/png", "version": "1" }
            ]})),
            Reply::json(json!({ "files": [
                { "id": "txt1", "name": "leave.txt", "mimeType": "text/plain", "version": "3" }
            ]})),
        ],
    );
    drive.route("GET /drive/v3/files/doc1/export", Reply::text("text/markdown", "# Handbook\n\nWe start at nine."));
    drive.route("GET /drive/v3/files/txt1", Reply::text("text/plain", "Annual leave is 20 days."));
    let source = SourceConfig::new(
        "drive",
        "google_drive",
        json!({ "folder_id": "root", "base_url": format!("{}/drive/v3", drive.base) }),
    )
    .dataset("kb")
    .auth(AuthConfig::Oauth {
        token_url: format!("{}/token", drive.base),
        client_id: "desktop-app".into(),
        client_secret_env: None,
        refresh_token_env: "DRIVE_REFRESH".into(),
    });
    let rotated: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = Arc::clone(&rotated);
    let secrets = |name: &str| (name == "DRIVE_REFRESH").then(|| "old-refresh".to_owned());
    let mut ctx = BuildContext::new(&secrets);
    ctx.private_service_endpoints = true;
    ctx.on_rotate = Some(Arc::new(move |token| {
        *seen.lock().unwrap() = Some(token.refresh_token.expose().to_owned());
    }));
    let connector = ConnectorRegistry::with_builtins().build(&source, &ctx).unwrap();
    let (kb, sink) = knowledge();
    let state = MemorySyncState::new();
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded, "{status:?}");
    assert_eq!(status.report.indexed, 2, "the image is not indexable");
    let text = all_text(&kb).await;
    assert!(text.contains("We start at nine") && text.contains("20 days"));
    assert_eq!(rotated.lock().unwrap().as_deref(), Some("rotated-refresh"));
    let token_requests: Vec<_> = drive.requests().into_iter().filter(|r| r.target == "/token").collect();
    assert!(
        token_requests[0].body.contains("refresh_token=old-refresh")
            && token_requests[0].body.contains("client_id=desktop-app")
    );
    let api_calls: Vec<_> = drive.requests().into_iter().filter(|r| r.target.starts_with("/drive")).collect();
    assert!(
        api_calls
            .iter()
            .skip(1)
            .all(|r| r.headers.get("authorization").map(String::as_str) == Some("Bearer fresh-access"))
    );
    assert!(api_calls.iter().any(|r| r.target.contains("export?mimeType=text%2Fmarkdown")));

    // Same versions on the next sync: nothing is downloaded again.
    drive.clear_log();
    drive.route_sequence(
        "GET /drive/v3/files",
        vec![
            Reply::json(json!({ "files": [
                { "id": "doc1", "name": "Handbook", "mimeType": "application/vnd.google-apps.document", "version": "7" },
                { "id": "sub", "name": "Policies", "mimeType": "application/vnd.google-apps.folder" }
            ]})),
            Reply::json(json!({ "files": [] })),
        ],
    );
    let status = sync_source(&source, connector.as_ref(), &sink, &state, &SyncOptions::default()).await.unwrap();
    assert_eq!((status.report.unchanged, status.report.removed), (1, 1), "{status:?}");
    assert_eq!(drive.count("GET", "/drive/v3/files/doc1"), 0, "unchanged version: no export");
    assert!(!all_text(&kb).await.contains("20 days"), "the deleted file is gone");
}

#[tokio::test]
async fn notion_pages_with_nested_blocks_and_database_rows() {
    let notion = Mock::start().await;
    let page_id = "1429989fe8ac4effbc8f57f56486db54";
    let row_id = "2429989fe8ac4effbc8f57f56486db54";
    let db_id = "3429989fe8ac4effbc8f57f56486db54";
    let title = |text: &str| json!({ "Name": { "type": "title", "title": [{ "plain_text": text }] } });
    notion.route(
        &format!("GET /v1/pages/{page_id}"),
        Reply::json(json!({ "object": "page", "id": page_id, "last_edited_time": "2026-11-01T00:00:00Z",
                            "url": "https://www.notion.so/Opening-hours", "properties": title("Opening hours") })),
    );
    notion.route(
        &format!("GET /v1/blocks/{page_id}/children"),
        Reply::json(json!({ "results": [
            { "id": "b1", "type": "heading_2", "heading_2": { "rich_text": [{ "plain_text": "Weekdays" }] } },
            { "id": "b2", "type": "toggle", "has_children": true, "toggle": { "rich_text": [{ "plain_text": "Details" }] } }
        ], "has_more": false })),
    );
    notion.route(
        "GET /v1/blocks/b2/children",
        Reply::json(json!({ "results": [
            { "id": "b3", "type": "bulleted_list_item", "bulleted_list_item": { "rich_text": [{ "plain_text": "Monday to Friday 8 to 18" }] } }
        ], "has_more": false })),
    );
    notion.route(
        &format!("POST /v1/databases/{db_id}/query"),
        Reply::json(json!({ "results": [
            { "object": "page", "id": row_id, "last_edited_time": "2026-11-02T00:00:00Z",
              "properties": { "Name": { "type": "title", "title": [{ "plain_text": "Latte" }] },
                              "Price": { "type": "number", "number": 4.5 } } }
        ], "has_more": false })),
    );
    notion.route(
        &format!("GET /v1/pages/{row_id}"),
        Reply::json(json!({ "object": "page", "id": row_id, "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": "Latte" }] }, "Price": { "type": "number", "number": 4.5 } } })),
    );
    notion
        .route(&format!("GET /v1/blocks/{row_id}/children"), Reply::json(json!({ "results": [], "has_more": false })));
    let source = SourceConfig::new(
        "notion",
        "notion",
        json!({ "page_ids": [format!("https://www.notion.so/acme/Opening-hours-{page_id}")], "database_ids": [db_id],
                "base_url": format!("{}/v1", notion.base) }),
    )
    .dataset("kb")
    .auth(AuthConfig::Bearer { token_env: "NOTION_TOKEN".into() });
    let secrets = |name: &str| (name == "NOTION_TOKEN").then(|| "secret_abc".to_owned());
    let connector = build(&source, &secrets);
    let (kb, sink) = knowledge();
    let status = sync_source(&source, connector.as_ref(), &sink, &MemorySyncState::new(), &SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded, "{status:?}");
    assert_eq!(status.report.indexed, 2);
    let text = all_text(&kb).await;
    assert!(text.contains("Monday to Friday 8 to 18"), "nested blocks are read: {text}");
    assert!(text.contains("Price: 4.5"), "database properties are indexed: {text}");
    assert!(
        notion.requests().iter().all(|r| r.headers.get("notion-version").map(String::as_str) == Some("2022-06-28")
            && r.headers.get("authorization").map(String::as_str) == Some("Bearer secret_abc"))
    );
}

#[tokio::test]
async fn sharepoint_site_library_with_paging_and_folders() {
    let graph = Mock::start().await;
    graph.route("GET /v1.0/sites/contoso.sharepoint.com:/sites/Marketing:/drive", Reply::json(json!({ "id": "drv1" })));
    graph.route(
        "GET /v1.0/drives/drv1/root:/Policies:/children",
        Reply::json(json!({
            "value": [
                { "id": "f1", "name": "travel.md", "file": { "mimeType": "text/markdown" }, "cTag": "c1", "webUrl": "https://contoso.sharepoint.com/f1" },
                { "id": "d1", "name": "2026", "folder": { "childCount": 1 } }
            ],
            "@odata.nextLink": format!("{}/v1.0/drives/drv1/root:/Policies:/children?page=2", graph.base)
        })),
    );
    graph.route(
        "GET /v1.0/drives/drv1/root:/Policies:/children?page=2",
        Reply::json(json!({ "value": [ { "id": "f2", "name": "video.mp4", "file": { "mimeType": "video/mp4" }, "cTag": "c9" } ] })),
    );
    graph.route(
        "GET /v1.0/drives/drv1/items/d1/children",
        Reply::json(json!({ "value": [ { "id": "f3", "name": "expenses.txt", "file": { "mimeType": "text/plain" }, "cTag": "c3" } ] })),
    );
    graph.route(
        "GET /v1.0/drives/drv1/items/f1/content",
        Reply::text("text/markdown", "# Travel\n\nBook trains, not planes."),
    );
    graph.route("GET /v1.0/drives/drv1/items/f3/content", Reply::text("text/plain", "Expenses within 30 days."));
    let source = SourceConfig::new(
        "sp",
        "sharepoint",
        json!({ "site_url": "https://contoso.sharepoint.com/sites/Marketing", "folder_path": "/Policies",
                "base_url": format!("{}/v1.0", graph.base) }),
    )
    .dataset("kb")
    .auth(AuthConfig::Bearer { token_env: "GRAPH_TOKEN".into() });
    let secrets = |name: &str| (name == "GRAPH_TOKEN").then(|| "graph-token".to_owned());
    let connector = build(&source, &secrets);
    let (kb, sink) = knowledge();
    let status = sync_source(&source, connector.as_ref(), &sink, &MemorySyncState::new(), &SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(status.outcome, SyncOutcome::Succeeded, "{status:?}");
    assert_eq!(status.report.indexed, 2, "the video is skipped");
    let text = all_text(&kb).await;
    assert!(text.contains("Book trains") && text.contains("Expenses within 30 days"));
}

#[tokio::test]
async fn a_missing_secret_is_reported_by_name() {
    let source =
        SourceConfig::new("n", "notion", json!({})).auth(AuthConfig::Bearer { token_env: "NOTION_TOKEN".into() });
    let error = ConnectorRegistry::with_builtins().build(&source, &BuildContext::new(&|_| None)).unwrap_err();
    assert_eq!(error.kind(), "auth");
    assert!(error.to_string().contains("NOTION_TOKEN"));
}
