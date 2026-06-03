//! Tier B — Recorded/replayed RPC for the network read paths (run on every PR).
//!
//! The base-node read APIs (`get_tip_info`, `is_node_synced`) are part of the
//! frozen public contract (ledger D2) and perform a `GET /get_tip_info`. Here we
//! stand up a local `wiremock` server that returns the **committed** recorded JSON
//! (see `verify::fixture::rpc`), point the API at it via the `base_url` parameter,
//! and assert the responses parse into the expected `TipInfo` / sync flag.
//!
//! No real base node, fully deterministic. `make record-fixtures` refreshes the
//! committed captures if the upstream RPC shape changes.

use rust_lib_flutter_rust_wallet::api::base_node::{get_tip_info, is_node_synced};
use verify::fixture::rpc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spin up a mock base node serving the committed `get_tip_info` capture.
async fn mock_node(capture_file: &str) -> MockServer {
    let body = rpc::read_committed(capture_file).expect("read committed RPC capture");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get_tip_info"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn get_tip_info_parses_recorded_synced_response() {
    let server = mock_node(rpc::SYNCED_FILE).await;

    let tip = get_tip_info(server.uri())
        .await
        .expect("get_tip_info should parse the recorded response")
        .expect("recorded response has chain metadata");

    assert_eq!(tip.best_block_height, rpc::TIP_HEIGHT);
    assert_eq!(tip.pruning_horizon, rpc::PRUNING_HORIZON);
    assert_eq!(tip.pruned_height, rpc::PRUNED_HEIGHT);
    assert_eq!(tip.timestamp, rpc::TIP_TIMESTAMP);
    // The DTO renders the block hash as lowercase hex; the first byte is 0xAB.
    assert!(
        tip.best_block_hash.starts_with("ab"),
        "unexpected best_block_hash: {}",
        tip.best_block_hash
    );
}

#[tokio::test]
async fn is_node_synced_reads_the_synced_flag() {
    let synced_server = mock_node(rpc::SYNCED_FILE).await;
    assert!(
        is_node_synced(synced_server.uri())
            .await
            .expect("is_node_synced (synced)"),
        "synced capture must report is_synced = true"
    );

    let unsynced_server = mock_node(rpc::UNSYNCED_FILE).await;
    assert!(
        !is_node_synced(unsynced_server.uri())
            .await
            .expect("is_node_synced (unsynced)"),
        "unsynced capture must report is_synced = false"
    );
}
