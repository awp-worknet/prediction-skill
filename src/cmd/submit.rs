/// submit — submit a prediction to the coordinator.
///
/// Builds and signs the request, then POSTs to /api/v1/predictions.
/// Logs full request/response details for debugging.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info, log_warn};

pub struct SubmitArgs {
    pub market: String,
    pub prediction: String,
    pub tickets: u32,
    pub reasoning: String,
    pub limit_price: Option<f64>,
    pub dry_run: bool,
}

pub fn run(server_url: &str, args: SubmitArgs) -> Result<()> {
    log_info!(
        "submit: market={}, prediction={}, tickets={}, limit_price={:?}, reasoning_len={}, dry_run={}",
        args.market,
        args.prediction,
        args.tickets,
        args.limit_price,
        args.reasoning.len(),
        args.dry_run
    );

    // Validate direction
    if args.prediction != "up" && args.prediction != "down" {
        log_error!("submit: invalid direction '{}' (must be 'up' or 'down')", args.prediction);
        Output::error_with_debug(
            "Prediction must be 'up' or 'down'.",
            "INVALID_DIRECTION",
            "validation",
            false,
            "Use --prediction up or --prediction down.",
            json!({
                "provided_prediction": args.prediction,
                "valid_values": ["up", "down"],
            }),
            Internal {
                next_action: "fix_command".into(),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }

    const MIN_TICKETS: u32 = 100;
    if args.tickets < MIN_TICKETS {
        log_error!("submit: tickets {} below minimum {}", args.tickets, MIN_TICKETS);
        Output::error_with_debug(
            format!("Minimum order size is {} tickets. You specified {}.", MIN_TICKETS, args.tickets),
            "TICKETS_TOO_SMALL",
            "validation",
            false,
            format!("Use --tickets N where N >= {}. Small bets waste submission slots.", MIN_TICKETS),
            json!({
                "provided_tickets": args.tickets,
                "minimum_tickets": MIN_TICKETS,
            }),
            Internal {
                next_action: "fix_command".into(),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }

    if let Some(lp) = args.limit_price {
        if !(0.01..=0.99).contains(&lp) {
            log_error!("submit: invalid limit_price {} (must be 0.01-0.99)", lp);
            Output::error_with_debug(
                format!("limit-price must be between 0.01 and 0.99, got {lp}"),
                "INVALID_LIMIT_PRICE",
                "validation",
                false,
                "Use --limit-price 0.01 to 0.99.",
                json!({
                    "provided_limit_price": lp,
                    "valid_range": "0.01-0.99",
                }),
                Internal {
                    next_action: "fix_command".into(),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    }

    // Validate reasoning length client-side
    let reasoning_len = args.reasoning.len();
    if reasoning_len < 80 {
        log_error!("submit: reasoning too short ({} chars, minimum 80)", reasoning_len);
        Output::error_with_debug(
            format!("Reasoning too short: {} characters (minimum 80).", reasoning_len),
            "REASONING_TOO_SHORT",
            "validation",
            false,
            "Expand your reasoning to at least 80 characters with at least 2 sentences.",
            json!({
                "reasoning_length": reasoning_len,
                "minimum_length": 80,
            }),
            Internal {
                next_action: "fix_command".into(),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }
    if reasoning_len > 2000 {
        log_warn!("submit: reasoning is {} chars (max 2000), server may reject", reasoning_len);
    }

    let mut body = json!({
        "market_id": args.market,
        "prediction": args.prediction,
        "tickets": args.tickets,
        "reasoning": args.reasoning,
    });

    if let Some(lp) = args.limit_price {
        body["limit_price"] = json!(lp);
    }

    if args.dry_run {
        log_info!("submit: dry-run mode, not sending to server");
        Output::success(
            format!(
                "[dry-run] Would submit {} prediction for market {} with {} tickets.",
                args.prediction.to_uppercase(),
                args.market,
                args.tickets
            ),
            json!({
                "dry_run": true,
                "would_submit": body,
            }),
            Internal {
                next_action: "submit".into(),
                next_command: Some(format!(
                    "predict-agent submit --market {} --prediction {} --tickets {} --reasoning \"{}\"{}",
                    args.market,
                    args.prediction,
                    args.tickets,
                    args.reasoning.chars().take(50).collect::<String>(),
                    if args.limit_price.is_some() {
                        format!(" --limit-price {}", args.limit_price.unwrap())
                    } else {
                        String::new()
                    }
                )),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }

    log_debug!("submit: creating API client...");
    let client = ApiClient::new(server_url.to_string())?;

    // Build canonical body for signature (matches server's format)
    // Format: market_id|prediction|limit_price_or_none|tickets|sha256(reasoning)
    let reasoning_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(args.reasoning.as_bytes()))
    };
    let limit_price_str = args
        .limit_price
        .map(|p| format!("{}", p))
        .unwrap_or_else(|| "none".to_string());
    let canonical_body = format!(
        "{}|{}|{}|{}|{}",
        args.market, args.prediction, limit_price_str, args.tickets, reasoning_hash
    );
    log_debug!("submit: canonical body = {}", canonical_body);

    log_info!("submit: sending prediction to server...");
    let resp = match client.post_auth_with_canonical(canonical_body.as_bytes(), "/api/v1/predictions", &body) {
        Ok(v) => {
            log_info!("submit: prediction accepted by server");
            v
        }
        Err(e) => {
            let err_str = e.to_string();
            log_error!("submit: server rejected prediction: {}", err_str);
            // Parse error details from server response if present
            let (code, category, retryable, suggestion) = parse_server_error(&err_str);
            log_debug!(
                "submit: parsed error — code={}, category={}, retryable={}, suggestion={}",
                code,
                category,
                retryable,
                suggestion
            );

            Output::error_with_debug(
                format!("Submission failed: {}", extract_message(&err_str)),
                &code,
                &category,
                retryable,
                &suggestion,
                json!({
                    "raw_error": err_str,
                    "market_id": args.market,
                    "prediction": args.prediction,
                    "tickets": args.tickets,
                    "reasoning_length": args.reasoning.len(),
                    "limit_price": args.limit_price,
                    "server_url": server_url,
                }),
                Internal {
                    next_action: if retryable {
                        "retry".into()
                    } else {
                        "fix_command".into()
                    },
                    wait_seconds: if retryable { Some(30) } else { None },
                    next_command: Some("predict-agent context".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));

    // Extract key fields for user_message
    let direction = data
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or(&args.prediction);
    let filled = data
        .get("tickets_filled")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let total = args.tickets as i64;
    let status = data
        .get("order_status")
        .and_then(|v| v.as_str())
        .unwrap_or("open");
    let payout = data
        .get("payout_if_correct")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".to_string());

    let market_id_short = args.market.clone();

    log_info!(
        "submit: result — direction={}, status={}, filled={}/{}, payout_if_correct={}",
        direction,
        status,
        filled,
        total,
        payout
    );

    let user_msg = match status {
        "filled" => format!(
            "Submitted {} for {}. Filled {}/{} tickets. Payout if correct: {} chips.",
            direction.to_uppercase(),
            market_id_short,
            filled,
            total,
            payout
        ),
        "partial" => format!(
            "Submitted {} for {}. Partially filled {}/{} tickets. Unfilled tickets auto-refund at close.",
            direction.to_uppercase(),
            market_id_short,
            filled,
            total,
        ),
        _ => format!(
            "Submitted {} for {}. {} tickets queued (no immediate fill). Chips locked until market close.",
            direction.to_uppercase(),
            market_id_short,
            total
        ),
    };

    Output::success(
        user_msg,
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

/// Extract a human-readable message from the error string.
fn extract_message(err: &str) -> String {
    // Try to parse as JSON first (server error response)
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(err) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return msg.to_string();
        }
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            return msg.to_string();
        }
    }
    // Try to extract JSON from "HTTP 4xx: {json}" format
    if let Some(json_start) = err.find('{') {
        let json_part = &err[json_start..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_part) {
            if let Some(msg) = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                return msg.to_string();
            }
        }
    }
    err.to_string()
}

/// Parse structured error code/category/retryable from server error response.
fn parse_server_error(err: &str) -> (String, String, bool, String) {
    let try_parse = |json_str: &str| -> Option<(String, String, bool, String)> {
        let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
        let err_obj = v.get("error")?;
        let code = err_obj.get("code")?.as_str()?.to_string();
        let category = err_obj
            .get("category")
            .and_then(|c| c.as_str())
            .unwrap_or("unknown")
            .to_string();
        let retryable = err_obj
            .get("retryable")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        let suggestion = err_obj
            .get("suggestion")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        Some((code, category, retryable, suggestion))
    };

    // Try raw err as JSON
    if let Some(result) = try_parse(err) {
        return result;
    }
    // Try to find JSON portion after "HTTP 4xx: "
    if let Some(json_start) = err.find('{') {
        if let Some(result) = try_parse(&err[json_start..]) {
            return result;
        }
    }

    // Defaults based on common patterns
    if err.contains("RATE_LIMIT") || err.contains("429") {
        return (
            "RATE_LIMIT_EXCEEDED".into(),
            "rate_limit".into(),
            true,
            "Wait and retry.".into(),
        );
    }
    if err.contains("MARKET_CLOSED") {
        return (
            "MARKET_CLOSED".into(),
            "validation".into(),
            false,
            "Choose an open market.".into(),
        );
    }
    if err.contains("INSUFFICIENT_BALANCE") || err.contains("insufficient") {
        return (
            "INSUFFICIENT_BALANCE".into(),
            "validation".into(),
            false,
            "Reduce --tickets or wait for the next chip feed (every 4 hours).".into(),
        );
    }
    if err.contains("REASONING_DUPLICATE") || err.contains("duplicate") {
        return (
            "REASONING_DUPLICATE".into(),
            "validation".into(),
            false,
            "Write completely new analysis. Do not reuse or rephrase previous reasoning.".into(),
        );
    }
    if err.contains("SERVICE_UNAVAILABLE") || err.contains("503") {
        return (
            "SERVICE_UNAVAILABLE".into(),
            "dependency".into(),
            true,
            "Server dependency temporarily down. Wait a few seconds and retry.".into(),
        );
    }

    (
        "SUBMISSION_FAILED".into(),
        "unknown".into(),
        false,
        "Check the error details and retry.".into(),
    )
}
