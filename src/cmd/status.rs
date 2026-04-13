/// status — show current agent state.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info};

/// Round a decimal chip string to 2 decimal places for display.
fn format_chips(s: &str) -> String {
    s.parse::<f64>()
        .map(|n| format!("{:.2}", n))
        .unwrap_or_else(|_| s.to_string())
}

pub fn run(server_url: &str) -> Result<()> {
    log_info!("status: fetching agent status from {}", server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let resp = match client.get_auth("/api/v1/agents/me/status") {
        Ok(v) => v,
        Err(e) => {
            log_error!("status: failed to fetch: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch status: {e}"),
                "STATUS_FAILED",
                "network",
                true,
                "Check coordinator connectivity.",
                json!({
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                    "error_chain": format!("{e:#}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    next_command: Some("predict-agent status".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));

    let total_predictions = data
        .get("total_predictions")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let balance_raw = data
        .get("balance")
        .and_then(|v| v.as_str())
        .unwrap_or("0");
    let balance = format_chips(balance_raw);
    let persona = data
        .get("persona")
        .and_then(|v| v.as_str())
        .unwrap_or("none");

    log_info!(
        "status: total_predictions={}, balance={}, persona={}",
        total_predictions,
        balance,
        persona
    );
    log_debug!("status: full response data: {}", serde_json::to_string(&data).unwrap_or_default());

    Output::success(
        format!(
            "Agent status: {} total predictions, {} chips balance, persona: {}.",
            total_predictions, balance, persona
        ),
        data,
        Internal {
            next_action: "fetch_context".into(),
            next_command: Some("predict-agent context".into()),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
