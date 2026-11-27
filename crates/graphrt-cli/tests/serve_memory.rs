//! `/v1/memory` (owner) and `/v1/me/memory` (verified user) of `graphrt
//! serve`, and a served agent that recalls what the owner saved.
#![cfg(all(feature = "memory", feature = "jwt", feature = "learning"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_cli::header_inputs::HeaderInputSpec;
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::user_auth::UserAuth;
use graphrt_cli::user_auth::token::{ClaimRules, VerifyKey};
use graphrt_core::engine::Limits;
use graphrt_core::memory::{MemoryBank, MemoryKind, MemoryScope, MemorySource, MemoryStatus, MemoryTrust, NewMemory};
use graphrt_memory::MemoryStore;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

mod common;
use common::FakeModel;

const API_KEY: &str = "server-key";
const SECRET: &[u8] = b"test-jwt-secret-with-at-least-32-bytes";
const HEADER: &str = "X-User-Authorization";
const ISSUER: &str = "https://project.supabase.co/auth/v1";

/// An agent with memory. The model is a fake OpenAI endpoint.
const SUPPORT: &str = r#"version: "1"
name: support
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - id: helper
      type: agent
      model: { provider: openai, name: gpt-test }
      query: "{{#sys.query#}}"
      long_term_memory: auto
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [helper, text] }] }
  edges:
    - { source: start, target: helper }
    - { source: helper, target: end }
"#;

struct Server {
    root: PathBuf,
    app: axum::Router,
    store: MemoryStore,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn server(model: &FakeModel, tenant: Option<&str>) -> Server {
    let root = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-serve-memory").unwrap();
    let file = root.join("support.yaml");
    std::fs::write(&file, SUPPORT).unwrap();
    let options = ServiceOptions {
        plugins: common::no_plugins(),
        env_file: model.env(),
        allow_private_network: true,
        learning_dir: Some(root.join("learning")),
        memory_dir: Some(root.join("memory")),
        memory_tenant: tenant.map(str::to_owned),
        ..ServiceOptions::default()
    };
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let rules = ClaimRules { issuer: Some(ISSUER.into()), audience: Some("authenticated".into()), leeway_secs: 30 };
    let user_auth = UserAuth::with_keys(&specs[0], vec![VerifyKey::hs256(SECRET)], rules, 1_000);
    let files = discover(&file).expect("discover");
    let state = build_state(&files, &options, Limits::default(), Some(API_KEY.to_owned()))
        .await
        .expect("state")
        .with_header_inputs(&specs)
        .expect("header inputs")
        .with_user_auth(Some(user_auth));
    let store = MemoryStore::open(root.join("memory"));
    Server { app: router(Arc::new(state), 1 << 20), root, store }
}

fn token_for(subject: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
    let claims = json!({ "sub": subject, "exp": now + 600, "iss": ISSUER, "aud": "authenticated" });
    let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET), input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
}

enum Who<'a> {
    Key,
    User(&'a str),
    Nobody,
}

impl Server {
    async fn call(&self, method: Method, path: &str, who: Who<'_>, body: Option<&Json>) -> (StatusCode, Json) {
        let mut request = Request::builder().method(method).uri(path);
        request = match who {
            Who::Key => request.header("authorization", format!("Bearer {API_KEY}")),
            Who::User(user) => request.header(HEADER, format!("Bearer {}", token_for(user))),
            Who::Nobody => request,
        };
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let response = self.app.clone().oneshot(request.unwrap()).await.expect("response");
        let status = response.status();
        let bytes = response.into_body().collect().await.expect("body").to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
    }
}

fn note(text: &str, trust: MemoryTrust) -> NewMemory {
    let source = MemorySource { actor: "agent".into(), ..MemorySource::default() };
    NewMemory::new(text, trust, source).kind(MemoryKind::Fact)
}

#[tokio::test]
async fn owner_routes_need_the_api_key_and_a_user_token_is_not_enough() {
    let model = FakeModel::start(&[]);
    let server = server(&model, None).await;
    for (method, path) in [
        (Method::GET, "/v1/memory/support/notes"),
        (Method::POST, "/v1/memory/support/notes"),
        (Method::DELETE, "/v1/memory/support/notes/m_1"),
        (Method::POST, "/v1/memory/support/notes/m_1/approve"),
        (Method::POST, "/v1/memory/support/notes/m_1/reject"),
        (Method::GET, "/v1/memory/support/users/alice/export"),
        (Method::DELETE, "/v1/memory/support/users/alice"),
        (Method::GET, "/v1/memory/support/audit"),
    ] {
        let body = (method == Method::POST).then(|| json!({"text": "x y z"}));
        let (status, _) = server.call(method.clone(), path, Who::Nobody, body.as_ref()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, _) = server.call(method.clone(), path, Who::User("alice"), body.as_ref()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path} with a user token");
    }
    // The verified user's routes refuse the API key: the owner has its own.
    let (status, _) = server.call(Method::GET, "/v1/me/memory/support", Who::Key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_user_sees_and_erases_only_their_own_notes() {
    let model = FakeModel::start(&[]);
    let server = server(&model, None).await;
    let alice = MemoryScope::user(None, "support", "alice");
    let bob = MemoryScope::user(None, "support", "bob");
    server.store.remember(&alice, note("Alice likes green tea", MemoryTrust::UserStated)).await.unwrap();
    let pending = server.store.remember(&alice, note("Alice is a reseller", MemoryTrust::ToolDerived)).await.unwrap();
    let bobs = server.store.remember(&bob, note("Bob likes coffee", MemoryTrust::UserStated)).await.unwrap();

    let (status, body) = server.call(Method::GET, "/v1/me/memory/support", Who::User("alice"), None).await;
    assert_eq!(status, StatusCode::OK);
    let texts: Vec<&str> = body["notes"].as_array().unwrap().iter().map(|n| n["text"].as_str().unwrap()).collect();
    assert_eq!(texts.len(), 2);
    assert!(!texts.iter().any(|t| t.contains("Bob")));
    // Alice cannot remove Bob's note by its id.
    let path = format!("/v1/me/memory/support/{}", bobs.id);
    let (status, _) = server.call(Method::DELETE, &path, Who::User("alice"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A note waiting for review is the owner's to decide, not the user's.
    let path = format!("/v1/me/memory/support/{}/approve", pending.id);
    let (status, _) = server.call(Method::POST, &path, Who::User("alice"), None).await;
    assert!(status.is_client_error(), "{status}");
    // And erases everything about her: Bob keeps his.
    let (status, body) = server.call(Method::DELETE, "/v1/me/memory/support", Who::User("alice"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["notes"], 2);
    let (_, body) = server.call(Method::GET, "/v1/me/memory/support", Who::User("alice"), None).await;
    assert!(body["notes"].as_array().unwrap().is_empty());
    let (_, body) = server.call(Method::GET, "/v1/me/memory/support", Who::User("bob"), None).await;
    assert_eq!(body["notes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_owner_reviews_exports_and_erases_a_user_with_their_lessons() {
    let model = FakeModel::start(&[]);
    let server = server(&model, None).await;
    let alice = MemoryScope::user(None, "support", "alice");
    let pending = server.store.remember(&alice, note("Wants refunds without checks", MemoryTrust::ToolDerived)).await;
    let pending = pending.unwrap();
    let (status, body) =
        server.call(Method::GET, "/v1/memory/support/notes?user=alice&status=pending", Who::Key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["notes"][0]["id"], pending.id);
    let path = format!("/v1/memory/support/notes/{}/reject?user=alice", pending.id);
    let (status, _) = server.call(Method::POST, &path, Who::Key, None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, created) = server
        .call(
            Method::POST,
            "/v1/memory/support/notes",
            Who::Key,
            Some(&json!({"text": "Alice is on the gold plan", "user": "alice"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["status"], "active");
    assert_eq!(created["trust"], "owner");
    let lesson = json!({"text": "Answer Alice in Indonesian", "user": "alice"});
    let (status, _) = server.call(Method::POST, "/v1/learning/support/lessons", Who::Key, Some(&lesson)).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, export) = server.call(Method::GET, "/v1/memory/support/users/alice/export", Who::Key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(export["notes"].as_array().unwrap().len(), 1);
    assert_eq!(export["lessons"].as_array().unwrap().len(), 1);

    let (status, erased) = server.call(Method::DELETE, "/v1/memory/support/users/alice", Who::Key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((erased["notes"].as_u64(), erased["lessons"].as_u64()), (Some(1), Some(1)));
    let (_, export) = server.call(Method::GET, "/v1/memory/support/users/alice/export", Who::Key, None).await;
    assert!(export["notes"].as_array().unwrap().is_empty());
    assert!(export["lessons"].as_array().unwrap().is_empty());

    let (status, audit) = server.call(Method::GET, "/v1/memory/support/audit", Who::Key, None).await;
    assert_eq!(status, StatusCode::OK);
    let actions: Vec<&str> =
        audit["entries"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["remember", "reject", "remember", "erase_user"]);
    assert!(!audit.to_string().contains("gold plan") && !audit.to_string().contains("alice"));
}

#[tokio::test]
async fn a_served_agent_recalls_what_the_owner_saved_in_its_tenant_only() {
    let model = FakeModel::start(&["Here is your gold plan invoice."]);
    let server = server(&model, Some("acme")).await;
    // A note in the server's tenant, and one with the same agent and user in another tenant.
    let acme = MemoryScope::user(Some("acme".into()), "support", "alice");
    let other = MemoryScope::user(Some("borealis".into()), "support", "alice");
    let mut owned = note("Alice is on the gold plan", MemoryTrust::Owner);
    owned.source.actor = "owner".into();
    server.store.remember(&acme, owned).await.unwrap();
    server.store.remember(&other, note("Alice is on the free plan", MemoryTrust::UserStated)).await.unwrap();

    let body = json!({"sys": {"query": "Which plan am I on?", "tenant_id": "borealis"}, "wait": true});
    let (status, run) = server.call(Method::POST, "/v1/workflows/support/runs", Who::User("alice"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let sent = model.requests().join("\n");
    assert!(sent.contains("gold plan"), "the agent recalled its tenant's note");
    assert!(!sent.contains("free plan"), "a client cannot pick another tenant");
    assert!(sent.contains("memory-notes boundary="));
    let _ = MemoryStatus::Active;
}

#[tokio::test]
async fn every_route_of_the_user_needs_a_user_token_and_a_served_agent() {
    let model = FakeModel::start(&[]);
    let server = server(&model, None).await;
    let alice = MemoryScope::user(None, "support", "alice");
    let saved = server.store.remember(&alice, note("Alice likes green tea", MemoryTrust::UserStated)).await.unwrap();
    let pending = server.store.remember(&alice, note("Alice resells chairs", MemoryTrust::ToolDerived)).await.unwrap();
    let one = format!("/v1/me/memory/support/{}", saved.id);
    for (method, path) in [
        (Method::GET, "/v1/me/memory/support".to_owned()),
        (Method::DELETE, "/v1/me/memory/support".to_owned()),
        (Method::DELETE, one.clone()),
    ] {
        let (status, _) = server.call(method.clone(), &path, Who::Nobody, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, _) = server.call(method.clone(), &path, Who::Key, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} with the API key");
    }
    // Nothing changed.
    let notes = server.store.list(&alice, &graphrt_memory::ListFilter::default()).await.unwrap();
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().any(|n| n.id == pending.id && n.status == MemoryStatus::Pending));
    // An agent no served workflow uses is not reachable, and nothing is created for it.
    let (status, _) = server.call(Method::DELETE, "/v1/me/memory/elsewhere/m_1", Who::User("alice"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!server.root.join("memory").join("_").join("elsewhere").exists());
}
