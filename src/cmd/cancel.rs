/// cancel — cancel an open order and refund locked chips.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_error, log_info};

pub fn run(server_url: &str, order_id: i64) -> Result<()> {
    log_info!("cancel: cancelling order {} on {}", order_id, server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let resp = match client.delete_auth(&format!("/api/v1/orders/{}", order_id)) {
        Ok(v) => v,
        Err(e) => {
            let err_str = format!("{e}");
            log_error!("cancel: failed: {}", err_str);

            // Parse error details if available
            let (code, suggestion) = if err_str.contains("NOT_FOUND") {
                ("ORDER_NOT_FOUND", "Check the order ID with 'predict-agent orders'.")
            } else if err_str.contains("FORBIDDEN") {
                ("FORBIDDEN", "You can only cancel your own orders.")
            } else if err_str.contains("ORDER_NOT_CANCELLABLE") {
                ("ORDER_NOT_CANCELLABLE", "Only open or partially filled orders can be cancelled.")
            } else if err_str.contains("MARKET_CLOSED") {
                ("MARKET_CLOSED", "Cannot cancel orders on closed markets. Wait for resolution.")
            } else {
                ("CANCEL_FAILED", "Check the order ID and try again.")
            };

            Output::error_with_debug(
                format!("Failed to cancel order: {e}"),
                code,
                "validation",
                false,
                suggestion,
                json!({
                    "order_id": order_id,
                    "error_detail": err_str,
                }),
                Internal {
                    next_action: "check_orders".into(),
                    next_command: Some("predict-agent orders --status open".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));
    let tickets_cancelled = data.get("tickets_cancelled").and_then(|v| v.as_i64()).unwrap_or(0);
    let chips_refunded = data.get("chips_refunded").and_then(|v| v.as_str()).unwrap_or("0");
    let new_balance = data.get("balance").and_then(|v| v.as_str()).unwrap_or("0");

    log_info!(
        "cancel: order {} cancelled, {} tickets cancelled, {} chips refunded, new balance: {}",
        order_id,
        tickets_cancelled,
        chips_refunded,
        new_balance
    );

    Output::success(
        format!(
            "Order {} cancelled. {} tickets cancelled, {} chips refunded. New balance: {}.",
            order_id,
            tickets_cancelled,
            chips_refunded,
            new_balance
        ),
        json!({
            "order_id": order_id,
            "tickets_cancelled": tickets_cancelled,
            "chips_refunded": chips_refunded,
            "balance": new_balance,
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
