//! The gateway (`graphrt-gateway`) and the agent (`serve --trust-gateway`)
//! implement `Graphrt-Sig` separately; each must accept the other's signatures.

#![cfg(all(feature = "gateway", feature = "trust-gateway"))]
#![allow(clippy::unwrap_used)]

use graphrt_cli::gateway_agent::signature as agent;
use graphrt_gateway::signature as gateway;

const SECRET: [u8; 32] = [7; 32];

#[test]
fn gateway_signatures_verify_on_the_agent_and_back() {
    let body = br#"{"workflow":"agent","event_index":0}"#;
    let now = gateway::unix_now();
    let gateway_signed = gateway::Signed {
        deployment: "shop",
        method: "POST",
        path: "/internal/v1/runs",
        run_id: "run-1",
        epoch: "1",
        body,
    };
    let header = gateway::sign(&gateway::GatewayKey::new("k1", &SECRET).unwrap(), now, &gateway_signed);
    let agent_signed = agent::Signed {
        deployment: "shop",
        method: "POST",
        path: "/internal/v1/runs",
        run_id: "run-1",
        epoch: "1",
        body,
    };
    let agent_verifier = agent::Verifier::new(vec![agent::GatewayKey::new("k1", &SECRET).unwrap()]);
    assert!(agent_verifier.verify(Some(&header), &agent_signed, now).is_ok(), "agent accepts the gateway's signature");
    let tampered = agent::Signed { epoch: "2", ..agent_signed };
    assert!(
        agent_verifier
            .verify(
                Some(&gateway::sign(&gateway::GatewayKey::new("k1", &SECRET).unwrap(), now, &gateway_signed)),
                &tampered,
                now
            )
            .is_err()
    );

    let orphan = br#"{"run_id":"run-1","epoch":1}"#;
    let agent_orphan = agent::Signed {
        deployment: "shop",
        method: "POST",
        path: "/internal/v1/orphans/run-1",
        run_id: "run-1",
        epoch: "1",
        body: orphan,
    };
    let header = agent::sign(&agent::GatewayKey::new("k1", &SECRET).unwrap(), now, &agent_orphan);
    let gateway_orphan = gateway::Signed {
        deployment: "shop",
        method: "POST",
        path: "/internal/v1/orphans/run-1",
        run_id: "run-1",
        epoch: "1",
        body: orphan,
    };
    let gateway_verifier = gateway::Verifier::new(vec![gateway::GatewayKey::new("k1", &SECRET).unwrap()]);
    assert!(
        gateway_verifier.verify(Some(&header), &gateway_orphan, now).is_ok(),
        "gateway accepts the agent's orphan report"
    );
}
