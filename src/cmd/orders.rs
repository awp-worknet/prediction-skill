/// orders — list agent's orders (open, filled, cancelled).

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info};

pub fn run(server_url: &str, market: Option<String>, status: &str, limit: u32) -> Result<()> {
    log_info!("orders: fetching orders (status={}, limit={}) from {}", status, limit, server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let mut url = format!("/api/v1/orders/me?limit={}&status={}", limit, status);
    if let Some(ref mid) = market {
        url.push_str(&format!("&market_id={}", mid));
    }

    let resp = match client.get_auth(&url) {
        Ok(v) => v,
        Err(e) => {
            log_error!("orders: failed to fetch: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch orders: {e}"),
                "ORDERS_FAILED",
                "network",
                true,
                "Check coordinator connectivity.",
                json!({
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    next_command: Some("predict-agent orders".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let orders = resp
        .get("data")
        .and_then(|d| d.get("orders"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let summary = resp
        .get("data")
        .and_then(|d| d.get("summary"))
        .cloned()
        .unwrap_or(json!({}));

    let open_count = summary.get("open").and_then(|v| v.as_i64()).unwrap_or(0);
    let total_pending = summary.get("total_pending_tickets").and_then(|v| v.as_i64()).unwrap_or(0);

    log_info!(
        "orders: {} total, {} open orders with {} pending tickets",
        orders.len(),
        open_count,
        total_pending
    );

    // Format orders for display
    let formatted: Vec<serde_json::Value> = orders
        .iter()
        .map(|o| {
            let status = o.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
            let can_cancel = o.get("can_cancel").and_then(|v| v.as_bool()).unwrap_or(false);
            let tickets = o.get("tickets").and_then(|v| v.as_i64()).unwrap_or(0);
            let filled = o.get("tickets_filled").and_then(|v| v.as_i64()).unwrap_or(0);
            let pending = o.get("tickets_pending").and_then(|v| v.as_i64()).unwrap_or(0);

            json!({
                "id": o.get("id"),
                "market_id": o.get("market_id"),
                "asset": o.get("asset"),
                "window": o.get("window"),
                "direction": o.get("direction"),
                "limit_price": o.get("limit_price"),
                "tickets": tickets,
                "tickets_filled": filled,
                "tickets_pending": pending,
                "fill_ratio": format!("{}/{}", filled, tickets),
                "chips_locked": o.get("chips_locked"),
                "chips_used": o.get("chips_used"),
                "pnl": o.get("pnl"),
                "status": status,
                "market_status": o.get("market_status"),
                "can_cancel": can_cancel,
                "close_at": o.get("close_at"),
                "created_at": o.get("created_at"),
            })
        })
        .collect();

    let human_msg = if open_count > 0 {
        format!(
            "{} orders found. {} open with {} pending tickets. Use 'predict-agent cancel --order <id>' to cancel.",
            orders.len(),
            open_count,
            total_pending
        )
    } else {
        format!("{} orders found. No open orders to cancel.", orders.len())
    };

    Output::success(
        human_msg,
        json!({
            "orders": formatted,
            "summary": summary,
        }),
        Internal {
            next_action: if open_count > 0 { "review_or_cancel".into() } else { "fetch_context".into() },
            next_command: Some("predict-agent context".into()),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
