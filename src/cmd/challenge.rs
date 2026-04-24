/// challenge — fetch an SMHL challenge for a market.
///
/// The returned nonce must be passed to `submit --challenge-nonce ...`.
/// Reasoning submitted must satisfy all constraints in this challenge.
use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_error, log_info};

pub fn run(server_url: &str, market_id: &str) -> Result<()> {
    log_info!("challenge: fetching challenge for market={}", market_id);

    let client = ApiClient::new(server_url.to_string())?;
    let path = format!("/api/v1/challenge?market_id={}", market_id);

    let resp = match client.get_auth(&path) {
        Ok(v) => v,
        Err(e) => {
            log_error!("challenge: server rejected: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch challenge: {}", e),
                "CHALLENGE_FETCH_FAILED",
                "server",
                true,
                "Verify market_id is open and retry.",
                json!({"market_id": market_id, "error": e.to_string()}),
                Internal {
                    next_action: "retry".into(),
                    wait_seconds: Some(10),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));
    let nonce = data
        .get("nonce")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(180);

    log_info!("challenge: got nonce={} (expires in {}s)", nonce, expires_in);

    Output::success(
        format!(
            "Challenge issued for {}. Nonce expires in {}s. \
             Read the `prompt` string in `data` — your reasoning must satisfy \
             EVERY constraint described there. Pass the nonce back via --challenge-nonce.",
            market_id, expires_in
        ),
        data,
        Internal {
            next_action: "compose_reasoning_then_submit".into(),
            next_command: Some(format!(
                "predict-agent submit --market {} --prediction <up|down> --tickets N --reasoning \"...\" --challenge-nonce {}",
                market_id, nonce
            )),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
