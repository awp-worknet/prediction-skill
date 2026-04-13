/// context — core command: fetch agent status + markets + klines in one call.
///
/// This is the command the LLM calls every round after preflight passes.
/// Returns everything needed to make a prediction decision.

use anyhow::Result;
use chrono::Utc;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info, log_warn};

pub fn run(server_url: &str) -> Result<()> {
    log_info!("context: fetching decision context from {}", server_url);
    let client = ApiClient::new(server_url.to_string())?;

    // Fetch agent status
    log_debug!("context: fetching agent status...");
    let status_resp = match client.get_auth("/api/v1/agents/me/status") {
        Ok(v) => {
            log_debug!("context: agent status fetched");
            v
        }
        Err(e) => {
            log_error!("context: failed to fetch agent status: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch agent status: {e}"),
                "STATUS_FAILED",
                "network",
                true,
                "Check coordinator connectivity and retry.",
                json!({
                    "phase": "fetch_agent_status",
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                    "error_chain": format!("{e:#}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    wait_seconds: Some(10),
                    next_command: Some("predict-agent context".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let agent_data = status_resp.get("data").cloned().unwrap_or(json!({}));
    log_debug!(
        "context: agent balance={}, total_predictions={}",
        agent_data.get("balance").and_then(|v| v.as_str()).unwrap_or("?"),
        agent_data.get("total_predictions").and_then(|v| v.as_i64()).unwrap_or(-1)
    );

    // Fetch active markets
    log_debug!("context: fetching active markets...");
    let markets_resp = match client.get("/api/v1/markets/active") {
        Ok(v) => {
            log_debug!("context: markets fetched");
            v
        }
        Err(e) => {
            log_error!("context: failed to fetch markets: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch markets: {e}"),
                "MARKETS_FAILED",
                "network",
                true,
                "Check coordinator connectivity and retry.",
                json!({
                    "phase": "fetch_markets",
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                    "error_chain": format!("{e:#}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    wait_seconds: Some(10),
                    next_command: Some("predict-agent context".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let markets_arr = markets_resp
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    log_info!("context: {} active markets found", markets_arr.len());

    // Fetch my predictions to find already-submitted markets
    log_debug!("context: fetching my predictions...");
    let my_preds_result = client.get_auth("/api/v1/predictions/me?limit=200");
    let my_preds: Vec<String> = match &my_preds_result {
        Ok(v) => {
            let preds: Vec<String> = v
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|p| p.get("market_id").and_then(|m| m.as_str()).map(str::to_string))
                .collect();
            log_debug!("context: {} existing predictions found", preds.len());
            preds
        }
        Err(e) => {
            log_warn!("context: failed to fetch my predictions (continuing): {}", e);
            vec![]
        }
    };

    let now = Utc::now();

    // Annotate markets with derived fields
    let mut annotated: Vec<serde_json::Value> = markets_arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let close_at_str = m.get("close_at")?.as_str()?;
            let close_at: chrono::DateTime<Utc> = close_at_str.parse().ok()?;
            let closes_in = (close_at - now).num_seconds();
            if closes_in < 0 {
                log_debug!("context: skipping market {} (already closed, closes_in={}s)", id, closes_in);
                return None;
            }

            let already_submitted = my_preds.contains(&id);
            let up_tickets = m
                .get("up_tickets_filled")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let down_tickets = m
                .get("down_tickets_filled")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let total = up_tickets + down_tickets;
            let implied_up_prob = if total > 0 {
                up_tickets as f64 / total as f64
            } else {
                0.5
            };

            log_debug!(
                "context: market {} — asset={}, closes_in={}s, submitted={}, up/down={}/{}, implied_up={:.2}",
                id,
                m.get("asset").and_then(|v| v.as_str()).unwrap_or("?"),
                closes_in,
                already_submitted,
                up_tickets,
                down_tickets,
                implied_up_prob
            );

            Some(json!({
                "id": id,
                "asset": m.get("asset").and_then(|v| v.as_str()).unwrap_or(""),
                "window": m.get("window").and_then(|v| v.as_str()).unwrap_or(""),
                "question": m.get("question").and_then(|v| v.as_str()).unwrap_or(""),
                "closes_in_seconds": closes_in,
                "close_at": close_at_str,
                "implied_up_prob": (implied_up_prob * 100.0).round() / 100.0,
                "stats": {
                    "up_tickets": up_tickets,
                    "down_tickets": down_tickets,
                    "participant_count": m.get("participant_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    "prediction_count": m.get("prediction_count").and_then(|v| v.as_i64()).unwrap_or(0),
                },
                "already_submitted": already_submitted,
                "recommended": false,
            }))
        })
        .collect();

    // Recommendation logic: submittable = not submitted + closes in > 120s
    let mut submittable: Vec<&mut serde_json::Value> = annotated
        .iter_mut()
        .filter(|m| {
            !m.get("already_submitted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && m.get("closes_in_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    > 120
        })
        .collect();

    // Sort by closes_in_seconds ascending (most urgent first)
    submittable.sort_by_key(|m| {
        m.get("closes_in_seconds")
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MAX)
    });

    let submittable_ids: Vec<String> = submittable
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();

    log_info!(
        "context: {} markets total, {} submittable (not submitted, >120s remaining)",
        annotated.len(),
        submittable_ids.len()
    );
    if !submittable_ids.is_empty() {
        log_debug!("context: submittable markets: {:?}", submittable_ids);
    }

    let recommended_id = submittable_ids.first().cloned();

    // Mark recommended market
    if let Some(ref rec_id) = recommended_id {
        log_info!("context: recommended market = {}", rec_id);
        for m in annotated.iter_mut() {
            if m.get("id").and_then(|v| v.as_str()) == Some(rec_id.as_str()) {
                m["recommended"] = json!(true);
            }
        }
    }

    // Fetch klines for recommended market and transform to structured format
    let (klines, klines_market_id) = if let Some(ref rec_id) = recommended_id {
        log_debug!("context: fetching klines for market {}...", rec_id);
        match client.get(&format!("/api/v1/markets/{}/klines", rec_id)) {
            Ok(resp) => {
                let raw = resp
                    .get("data")
                    .and_then(|d| d.get("klines"))
                    .and_then(|k| k.as_array());
                let candle_count = raw.map(|a| a.len()).unwrap_or(0);
                log_info!("context: received {} kline candles for {}", candle_count, rec_id);

                let structured = raw.map(|arr| {
                    arr.iter()
                        .filter_map(|candle| {
                            // Server returns Kline structs as JSON objects with named fields
                            if let Some(obj) = candle.as_object() {
                                let ts_ms = obj.get("open_time")
                                    .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))?;
                                let time = chrono::DateTime::from_timestamp(ts_ms / 1000, 0)
                                    .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                                    .unwrap_or_default();
                                let get_f64 = |key: &str| -> f64 {
                                    obj.get(key)
                                        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                                        .unwrap_or(0.0)
                                };
                                return Some(json!({
                                    "time": time,
                                    "open": get_f64("open"),
                                    "high": get_f64("high"),
                                    "low": get_f64("low"),
                                    "close": get_f64("close"),
                                    "volume": get_f64("volume"),
                                }));
                            }
                            // Fallback: raw Binance array format [timestamp, open, high, low, close, volume, ...]
                            if let Some(c) = candle.as_array() {
                                if c.len() < 6 {
                                    log_warn!("context: skipping malformed candle (len={}): {:?}", c.len(), c);
                                    return None;
                                }
                                let ts_ms = c[0].as_i64().or_else(|| c[0].as_f64().map(|f| f as i64))?;
                                let time = chrono::DateTime::from_timestamp(ts_ms / 1000, 0)
                                    .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                                    .unwrap_or_default();
                                return Some(json!({
                                    "time": time,
                                    "open": c[1].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                                    "high": c[2].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                                    "low": c[3].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                                    "close": c[4].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                                    "volume": c[5].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                                }));
                            }
                            log_warn!("context: skipping unrecognized candle format: {:?}", candle);
                            None
                        })
                        .collect::<Vec<_>>()
                });
                // Get asset and window from the recommended market for metadata
                let rec_market = annotated
                    .iter()
                    .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(rec_id.as_str()));
                let asset = rec_market
                    .and_then(|m| m.get("asset"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let window = rec_market
                    .and_then(|m| m.get("window"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let interval = match window {
                    "15m" => "1m",
                    "30m" => "5m",
                    "1h" => "15m",
                    _ => "1m",
                };
                let klines_data = structured.map(|data| {
                    json!({
                        "asset": asset,
                        "interval": interval,
                        "candles": data,
                    })
                });
                (klines_data, Some(rec_id.clone()))
            }
            Err(e) => {
                log_warn!("context: failed to fetch klines for {} (continuing without): {}", rec_id, e);
                (None, None)
            }
        }
    } else {
        log_debug!("context: no recommended market, skipping klines fetch");
        (None, None)
    };

    // Determine action
    // Note: daily rate limit is enforced server-side. Client cannot check today's count
    // because agent status only returns total_predictions (cumulative).
    // If the server rejects with RATE_LIMIT_EXCEEDED, the error handler will surface it.
    let (action, reason, wait_seconds) = if submittable_ids.is_empty() {
        log_info!("context: no submittable markets, action=wait");
        (
            "wait",
            "No submittable markets (all submitted or closing too soon). Wait for new markets.",
            Some(300u64),
        )
    } else {
        log_info!("context: action=submit, {} markets available", submittable_ids.len());
        (
            "submit",
            "Markets available. Analyze klines and submit your prediction.",
            None,
        )
    };

    let recommendation = json!({
        "action": action,
        "market_id": recommended_id,
        "reason": reason,
    });

    let next_command = if action == "submit" {
        recommended_id.as_ref().map(|id| {
            format!(
                "predict-agent submit --market {} --prediction <up|down> --tickets <N> --reasoning \"<your analysis>\"",
                id
            )
        })
    } else {
        Some("predict-agent context".into())
    };

    let klines_section = match (klines, klines_market_id) {
        (Some(data), Some(id)) => {
            let mut obj = data;
            obj["market_id"] = json!(id);
            obj
        }
        _ => json!(null),
    };

    let user_msg = if action == "submit" {
        format!(
            "{} submittable markets. Recommended: {}.",
            submittable_ids.len(),
            recommended_id.as_deref().unwrap_or("none")
        )
    } else {
        reason.to_string()
    };

    Output::success(
        user_msg,
        json!({
            "agent": agent_data,
            "markets": annotated,
            "klines": klines_section,
            "recommendation": recommendation,
        }),
        Internal {
            next_action: action.to_string(),
            next_command,
            wait_seconds,
            submittable_markets: if submittable_ids.is_empty() {
                None
            } else {
                Some(submittable_ids)
            },
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
