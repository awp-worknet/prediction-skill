/// history — show recent prediction history for this agent.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info};

pub fn run(server_url: &str, limit: u32) -> Result<()> {
    log_info!("history: fetching last {} predictions from {}", limit, server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let resp = match client.get_auth(&format!("/api/v1/predictions/me?limit={}", limit)) {
        Ok(v) => v,
        Err(e) => {
            log_error!("history: failed to fetch: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch history: {e}"),
                "HISTORY_FAILED",
                "network",
                true,
                "Check coordinator connectivity.",
                json!({
                    "server_url": server_url,
                    "limit": limit,
                    "error_detail": format!("{e}"),
                    "error_chain": format!("{e:#}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    next_command: Some(format!("predict-agent history --limit {}", limit)),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let preds = resp
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let count = preds.len();
    let correct: usize = preds
        .iter()
        .filter(|p| {
            p.get("payout_chips")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|n| n > 0.0)
                .unwrap_or(false)
        })
        .count();

    let accuracy = if count > 0 {
        (correct as f64 / count as f64 * 100.0).round() / 100.0
    } else {
        0.0
    };

    log_info!(
        "history: {} predictions returned, {} correct ({:.1}% accuracy)",
        count,
        correct,
        accuracy * 100.0
    );
    log_debug!(
        "history: markets = {:?}",
        preds
            .iter()
            .filter_map(|p| p.get("market_id").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
    );

    Output::success(
        format!(
            "Last {} predictions. {} correct ({:.1}% accuracy).",
            count,
            correct,
            accuracy * 100.0
        ),
        json!({
            "predictions": preds,
            "summary": {
                "count": count,
                "correct": correct,
                "accuracy": accuracy,
            }
        }),
        Internal {
            next_action: "fetch_context".into(),
            next_command: Some("predict-agent context".into()),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
