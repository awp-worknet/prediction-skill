/// result -- query the outcome of a specific market and this agent's prediction.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info};

pub fn run(server_url: &str, market_id: &str) -> Result<()> {
    log_info!("result: querying market {} from {}", market_id, server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let market_resp = match client.get(&format!("/api/v1/markets/{}", market_id)) {
        Ok(v) => v,
        Err(e) => {
            log_error!("result: market {} not found or fetch failed: {}", market_id, e);
            Output::error_with_debug(
                format!("Market {} not found: {}", market_id, e),
                "MARKET_NOT_FOUND",
                "validation",
                false,
                "Check market ID. Use: predict-agent history",
                json!({
                    "market_id": market_id,
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                }),
                Internal {
                    next_action: "fetch_context".into(),
                    next_command: Some("predict-agent context".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let market = market_resp.get("data").cloned().unwrap_or(json!({}));
    let status = market
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    log_info!("result: market {} status={}", market_id, status);

    if status != "resolved" {
        let closes_at = market
            .get("close_at")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        log_info!("result: market {} not yet resolved (closes_at={})", market_id, closes_at);
        Output::success(
            format!(
                "Market {} is still {}. Check back after it resolves (closes at {}).",
                market_id, status, closes_at
            ),
            json!({
                "market_id": market_id,
                "status": status,
                "close_at": closes_at,
                "outcome": null,
            }),
            Internal {
                next_action: "wait".into(),
                next_command: Some(format!("predict-agent result --market {}", market_id)),
                wait_seconds: Some(60),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }

    let outcome = market
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let open_price = market
        .get("open_price")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let resolve_price = market
        .get("resolve_price")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    log_info!(
        "result: market {} resolved {} (open={}, resolve={})",
        market_id,
        outcome,
        open_price,
        resolve_price
    );

    // Fetch my prediction for this market
    log_debug!("result: fetching my predictions to find submission for {}", market_id);
    let my_preds = client
        .get_auth("/api/v1/predictions/me?limit=500")
        .ok()
        .and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default();

    let my_pred = my_preds
        .iter()
        .find(|p| p.get("market_id").and_then(|m| m.as_str()) == Some(market_id));

    let (user_msg, result_data) = match my_pred {
        Some(pred) => {
            let direction = pred
                .get("direction")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let correct = direction == outcome;
            let payout = pred
                .get("payout_chips")
                .and_then(|v| v.as_str())
                .unwrap_or("0");
            let filled = pred
                .get("tickets_filled")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            log_info!(
                "result: your prediction={}, outcome={}, correct={}, payout={}, filled={}",
                direction,
                outcome,
                correct,
                payout,
                filled
            );

            let msg = if correct {
                format!(
                    "Market {} resolved {}. You predicted {} — CORRECT! Payout: {} chips ({} tickets filled).",
                    market_id,
                    outcome.to_uppercase(),
                    direction.to_uppercase(),
                    payout,
                    filled
                )
            } else {
                format!(
                    "Market {} resolved {}. You predicted {} — WRONG. No payout ({} tickets filled).",
                    market_id,
                    outcome.to_uppercase(),
                    direction.to_uppercase(),
                    filled
                )
            };

            (
                msg,
                json!({
                    "market_id": market_id,
                    "outcome": outcome,
                    "open_price": open_price,
                    "resolve_price": resolve_price,
                    "your_prediction": direction,
                    "correct": correct,
                    "tickets_filled": filled,
                    "payout_received": payout,
                }),
            )
        }
        None => {
            log_info!("result: no prediction submitted for market {}", market_id);
            (
                format!(
                    "Market {} resolved {}. You did not submit a prediction for this market.",
                    market_id,
                    outcome.to_uppercase()
                ),
                json!({
                    "market_id": market_id,
                    "outcome": outcome,
                    "open_price": open_price,
                    "resolve_price": resolve_price,
                    "your_prediction": null,
                    "correct": null,
                }),
            )
        }
    };

    Output::success(
        user_msg,
        result_data,
        Internal {
            next_action: "fetch_context".into(),
            next_command: Some("predict-agent context".into()),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
