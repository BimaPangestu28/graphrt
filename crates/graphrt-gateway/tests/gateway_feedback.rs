//! Feedback through the gateway (`POST /v1/deployments/{d}/runs/{id}/feedback`)
//! and who the gateway says started a run (`caller`).
//!
//! The gateway is the only party that verifies end-user tokens, so it decides
//! whose words feedback is: an end user rates only a run started with a token
//! of the same user, and is then its author whatever the body says
//! (`verified: true` for the agent); a deployment API key only labels feedback
//! (`verified: false`). The feedback tests need the cargo feature `feedback`. End-user tokens here are ES256, signed with a key made
//! for each test and served as a JWKS on loopback.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
// Without the `feedback` feature only the `caller` tests run.
#![cfg_attr(not(feature = "feedback"), allow(dead_code))]

mod common;

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{API_KEY, AgentWorld, DEPLOYMENT, Options, TestGateway, start_gateway};
use graphrt_core::store::RunStore as _;
use graphrt_gateway::auth::JwtConfig;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ISSUER: &str = "https://auth.shop.invalid";
const SARI: &str = "user-sari";
const BUDI: &str = "user-budi";

/// The identity provider of a test: one ES256 key, its JWKS on loopback.
struct Issuer {
    key: EcdsaKeyPair,
    jwks_url: String,
}

impl Issuer {
    async fn start() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        // An uncompressed point: 0x04, then x and y.
        let point = key.public_key().as_ref();
        let (x, y) = (URL_SAFE_NO_PAD.encode(&point[1..33]), URL_SAFE_NO_PAD.encode(&point[33..65]));
        let body = json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "test", "x": x, "y": y }] }).to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let jwks_url = format!("http://{}/jwks.json", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0_u8; 2048];
                let _read = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _written = stream.write_all(response.as_bytes()).await;
            }
        });
        Self { key, jwks_url }
    }

    fn config(&self) -> JwtConfig {
        JwtConfig {
            issuer: ISSUER.to_owned(),
            audience: DEPLOYMENT.to_owned(),
            jwks_url: Some(self.jwks_url.clone()),
            hs256_secret_env: None,
        }
    }

    /// A valid token of `subject` for the deployment.
    fn token(&self, subject: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(json!({ "alg": "ES256", "kid": "test", "typ": "JWT" }).to_string());
        let exp = graphrt_gateway::signature::unix_now() + 3600;
        let claims = json!({ "iss": ISSUER, "aud": DEPLOYMENT, "sub": subject, "exp": exp });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let signature = self.key.sign(&SystemRandom::new(), signing_input.as_bytes()).unwrap();
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

/// A gateway whose deployment takes the API key and the issuer's tokens.
async fn gateway(issuer: &Issuer) -> (TestGateway, Arc<AgentWorld>) {
    let world = Arc::new(AgentWorld::default());
    let gateway =
        start_gateway(Options { world: world.clone(), jwt: Some(issuer.config()), ..Options::default() }).await;
    (gateway, world)
}

/// Who calls the gateway.
#[derive(Clone, Copy)]
enum By<'a> {
    Nobody,
    Key,
    Token(&'a str),
}

async fn post(gateway: &TestGateway, path: &str, by: By<'_>, body: &Json) -> (u16, Json) {
    let mut request = gateway.client().post(format!("{}{path}", gateway.url)).json(body);
    request = match by {
        By::Nobody => request,
        By::Key => request.header("x-api-key", API_KEY),
        By::Token(token) => request.bearer_auth(token),
    };
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Json::Null))
}

/// Starts a run (sync) and returns its id.
async fn run(gateway: &TestGateway, by: By<'_>, body: Json) -> String {
    let (status, record) = post(gateway, &format!("/v1/deployments/{DEPLOYMENT}/runs"), by, &body).await;
    assert_eq!(status, 200, "{record}");
    record["run_id"].as_str().unwrap().to_owned()
}

fn feedback_path(run_id: &str) -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs/{run_id}/feedback")
}

fn thumbs_down() -> Json {
    json!({ "name": "thumbs", "value": false, "comment": "Look the order up first." })
}

fn received(world: &AgentWorld) -> Vec<(String, Json)> {
    world.feedback.lock().unwrap().clone()
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_users_feedback_on_their_own_run_reaches_the_agent_as_their_own_words() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    let sari = issuer.token(SARI);
    let run_id = run(&gateway, By::Token(&sari), json!({ "inputs": {} })).await;

    // She names Budi and claims nothing about verification: the gateway decides both.
    let mut body = thumbs_down();
    body["user"] = json!(BUDI);
    body["verified"] = json!(false);
    body["workflow"] = json!("another-workflow");
    let (status, score) = post(&gateway, &feedback_path(&run_id), By::Token(&sari), &body).await;
    assert_eq!(status, 201, "{score}");
    assert_eq!(score["user"], SARI, "{score}");

    let forwarded = received(&world);
    assert_eq!(forwarded.len(), 1);
    let (to_run, forwarded) = &forwarded[0];
    assert_eq!(to_run, &run_id);
    assert_eq!(forwarded["user"], SARI);
    assert_eq!(forwarded["verified"], true);
    assert_eq!(forwarded["workflow"], "agent", "the run's workflow, from the gateway's record");
    assert_eq!(forwarded["comment"], "Look the order up first.");
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_without_a_valid_token_or_key_is_refused() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    let run_id = run(&gateway, By::Token(&issuer.token(SARI)), json!({ "inputs": {} })).await;

    let other_issuer = Issuer::start().await;
    let forged = other_issuer.token(SARI);
    for by in [By::Nobody, By::Token(&forged), By::Token("not-a-token")] {
        let (status, refused) = post(&gateway, &feedback_path(&run_id), by, &thumbs_down()).await;
        assert_eq!(status, 401, "{refused}");
    }
    assert!(received(&world).is_empty(), "a refused caller reached the agent");
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_users_token_cannot_rate_someone_elses_run() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    let (sari, budi) = (issuer.token(SARI), issuer.token(BUDI));
    let run_id = run(&gateway, By::Token(&sari), json!({ "inputs": {} })).await;

    let mut as_sari = thumbs_down();
    as_sari["user"] = json!(SARI);
    for body in [thumbs_down(), as_sari] {
        let (status, refused) = post(&gateway, &feedback_path(&run_id), By::Token(&budi), &body).await;
        assert_eq!(status, 403, "{refused}");
    }
    // A run of another deployment, or none at all, is not found.
    let (status, _) = post(&gateway, &feedback_path("no-such-run"), By::Token(&sari), &thumbs_down()).await;
    assert_eq!(status, 404);
    assert!(received(&world).is_empty());
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_the_api_key_started_is_no_users_even_when_it_names_them() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    // The key holder labels the run as Sari's and sends a `caller` of its own.
    let run_id = run(&gateway, By::Key, json!({ "inputs": {}, "user_id": SARI, "caller": "user" })).await;
    let stored = gateway.gateway.store.get(&run_id).await.unwrap().unwrap();
    assert_eq!(stored.user_id.as_deref(), Some(SARI));
    assert!(!stored.user_verified, "a label, not a verified user");
    let dispatched = world.bodies.lock().unwrap().last().cloned().unwrap();
    assert_eq!(dispatched["caller"], "key", "the gateway says who called, over what the client sent");

    let (status, refused) =
        post(&gateway, &feedback_path(&run_id), By::Token(&issuer.token(SARI)), &thumbs_down()).await;
    assert_eq!(status, 403, "{refused}");
    assert!(received(&world).is_empty());
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_with_the_api_key_is_a_label_whatever_it_claims() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    let run_id = run(&gateway, By::Token(&issuer.token(SARI)), json!({ "inputs": {} })).await;

    let mut body = thumbs_down();
    body["user"] = json!(SARI);
    body["verified"] = json!(true);
    let (status, score) = post(&gateway, &feedback_path(&run_id), By::Key, &body).await;
    assert_eq!(status, 201, "{score}");
    let forwarded = received(&world);
    assert_eq!(forwarded[0].1["verified"], false, "{:?}", forwarded[0].1);
    assert_eq!(forwarded[0].1["user"], SARI, "a label the key holder chose");
}

#[cfg(feature = "feedback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agents_refusal_reaches_the_client() {
    let issuer = Issuer::start().await;
    let (gateway, _world) = gateway(&issuer).await;
    let sari = issuer.token(SARI);
    let run_id = run(&gateway, By::Token(&sari), json!({ "inputs": {} })).await;
    let (status, refused) =
        post(&gateway, &feedback_path(&run_id), By::Token(&sari), &json!({ "name": "refuse-me" })).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "`name` must be 1-200 bytes");
    // Not a JSON object.
    let (status, _) = post(&gateway, &feedback_path(&run_id), By::Token(&sari), &json!([1])).await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gateway_says_who_started_each_run() {
    let issuer = Issuer::start().await;
    let (gateway, world) = gateway(&issuer).await;
    let sari = issuer.token(SARI);
    let run_id =
        run(&gateway, By::Token(&sari), json!({ "inputs": {}, "caller": "key", "sys": { "user_id": BUDI } })).await;
    let dispatched = world.bodies.lock().unwrap().last().cloned().unwrap();
    assert_eq!((dispatched["caller"].as_str(), dispatched["user_id"].as_str()), (Some("user"), Some(SARI)));
    assert_eq!(dispatched["sys"]["user_id"], SARI);
    let stored = gateway.gateway.store.get(&run_id).await.unwrap().unwrap();
    assert!(stored.user_verified, "{stored:?}");
    // The stored request keeps it, so a re-dispatch after a restart says it too.
    let request = gateway.gateway.store.load_request(&run_id).await.unwrap().unwrap();
    assert!(request.contains(r#""caller":"user""#), "{request}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_abort_and_feedback_share_one_run_route() {
    let issuer = Issuer::start().await;
    let (gateway, _world) = gateway(&issuer).await;
    let run_id = run(&gateway, By::Key, json!({ "inputs": { "mode": "hitl" } })).await;
    let path = |action: &str| format!("/v1/deployments/{DEPLOYMENT}/runs/{run_id}/{action}");

    let (status, _) = post(&gateway, &path("delete"), By::Key, &json!({})).await;
    assert_eq!(status, 404, "an unknown action");
    let (status, _) = post(&gateway, &path("abort"), By::Nobody, &json!({})).await;
    assert_eq!(status, 401);
    let (status, aborted) = post(&gateway, &path("abort"), By::Key, &json!({})).await;
    assert_eq!((status, aborted["status"].as_str()), (202, Some("aborting")), "{aborted}");
    let stored = gateway.gateway.store.get(&run_id).await.unwrap().unwrap();
    assert_eq!(stored.status, graphrt_core::store::RunStatus::Aborted);
    let (status, refused) = post(&gateway, &path("resume"), By::Key, &json!({})).await;
    assert_eq!(status, 409, "an aborted run does not resume: {refused}");
}
