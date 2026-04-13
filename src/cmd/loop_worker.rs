/// loop_worker — background prediction loop.
///
/// Runs continuously: fetch context → call LLM for analysis → submit prediction → sleep.
///
/// LLM is invoked via OpenClaw CLI with extended thinking:
///   `openclaw agent --agent <id> --message <prompt> --thinking high --timeout 180`
///
/// With --thinking high, the agent can:
///   - Do deeper reasoning before making predictions
///   - Use web search to check news, sentiment, market data (if configured)
///   - Use any tools available in the agent's gateway configuration
///   - Output a final `DECISION: {...}` with its prediction
///
/// Usage: predict-agent loop [--interval 120] [--max-iterations 0] [--agent-id predict-worker]
///
/// The loop handles:
///   - Automatic context fetching each round
///   - LLM prompt construction with klines data
///   - Parsing LLM response (extracts DECISION: JSON from output)
///   - Submission with error recovery
///   - Adaptive backoff on empty markets or errors
///   - Graceful shutdown on SIGINT/SIGTERM

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::auth::refresh_wallet_token;
use crate::client::ApiClient;
use crate::{log_debug, log_error, log_info, log_warn};

pub struct LoopArgs {
    pub interval: u64,
    pub max_iterations: u64,
    pub agent_id: String,
    /// If true, output [NOTIFY] lines for the agent to relay to user
    pub notify: bool,
}

/// Print a notification line that the agent should relay to the user.
/// Format: [NOTIFY] <message>
/// Only printed if notify=true.
macro_rules! notify {
    ($notify:expr, $($arg:tt)*) => {
        if $notify {
            println!("[NOTIFY] {}", format!($($arg)*));
        }
    };
}

pub fn run(server_url: &str, args: LoopArgs) -> Result<()> {
    log_info!(
        "loop: starting (interval={}s, max_iter={}, agent={}, server={})",
        args.interval,
        args.max_iterations,
        args.agent_id,
        server_url
    );

    // Set up graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        eprintln!("\n[predict-agent] loop: received shutdown signal, finishing current round...");
        r.store(false, Ordering::SeqCst);
    })
    .ok(); // Ignore error if handler already set

    // Detect OpenClaw CLI
    let openclaw_bin = detect_openclaw();
    if openclaw_bin.is_none() {
        log_error!("loop: openclaw CLI not found. Install OpenClaw or add it to PATH.");
        log_error!("loop: the prediction loop requires an LLM to analyze markets and generate reasoning.");
        eprintln!("\npredict-agent loop requires the OpenClaw CLI (openclaw) to be installed.");
        eprintln!("The loop calls an LLM each round to analyze klines and write original reasoning.");
        eprintln!("\nInstall: https://docs.openclaw.com/install");
        return Ok(());
    }
    let openclaw_bin = openclaw_bin.unwrap();
    log_info!("loop: using openclaw at {}", openclaw_bin);

    // Ensure agent exists
    ensure_agent(&openclaw_bin, &args.agent_id);

    let mut iteration: u64 = 0;
    let mut consecutive_empty = 0u32;
    let mut consecutive_errors = 0u32;

    while running.load(Ordering::SeqCst) {
        iteration += 1;
        if args.max_iterations > 0 && iteration > args.max_iterations {
            log_info!("loop: reached max iterations ({}), stopping", args.max_iterations);
            break;
        }

        log_info!("loop: === iteration {} ===", iteration);
        let iter_start = Instant::now();

        match run_iteration(server_url, &openclaw_bin, &args.agent_id) {
            IterationResult::Submitted { market, direction, tickets, tickets_filled, order_status } => {
                let elapsed = iter_start.elapsed().as_secs_f64();
                let fill_info = match order_status.as_str() {
                    "filled" => format!("FILLED {}/{}", tickets_filled, tickets),
                    "partial" => format!("PARTIAL {}/{}", tickets_filled, tickets),
                    _ => format!("PENDING 0/{} (waiting for counterparty)", tickets),
                };
                log_info!("loop: {} {} for {} — {} ({:.1}s)", direction, fill_info, market, order_status, elapsed);
                notify!(
                    args.notify,
                    "Round {}: {} {} — {} ({:.1}s)",
                    iteration,
                    direction.to_uppercase(),
                    market,
                    fill_info,
                    elapsed
                );
                consecutive_empty = 0;
                consecutive_errors = 0;
            }
            IterationResult::Skipped { reason } => {
                let elapsed = iter_start.elapsed().as_secs_f64();
                log_info!("loop: skipped this round ({:.1}s): {}", elapsed, reason);
                notify!(args.notify, "Round {}: Skipped — {}", iteration, reason);
                consecutive_empty = 0;
                consecutive_errors = 0;
                // No penalty for skipping — it's a valid decision
            }
            IterationResult::NoMarkets { wait_seconds } => {
                consecutive_empty += 1;
                let backoff = calculate_backoff(args.interval, consecutive_empty, Some(wait_seconds));
                log_info!(
                    "loop: no submittable markets (consecutive={}), sleeping {}s",
                    consecutive_empty,
                    backoff
                );
                notify!(args.notify, "Round {}: No markets available, waiting {}s", iteration, backoff);
                interruptible_sleep(backoff, &running);
                continue;
            }
            IterationResult::RateLimited { wait_seconds } => {
                log_info!("loop: rate limited, sleeping {}s", wait_seconds);
                notify!(args.notify, "Round {}: Rate limited, waiting {}s", iteration, wait_seconds);
                interruptible_sleep(wait_seconds, &running);
                continue;
            }
            IterationResult::LlmFailed { reason } => {
                consecutive_errors += 1;
                let backoff = calculate_backoff(args.interval, consecutive_errors, None);
                log_warn!(
                    "loop: LLM call failed ({}), sleeping {}s (errors={})",
                    reason,
                    backoff,
                    consecutive_errors
                );
                notify!(args.notify, "Round {}: LLM error — {}, retrying in {}s", iteration, reason, backoff);
                interruptible_sleep(backoff, &running);
                continue;
            }
            IterationResult::Error { reason } => {
                consecutive_errors += 1;
                let backoff = calculate_backoff(args.interval, consecutive_errors, None);
                log_error!(
                    "loop: iteration error ({}), sleeping {}s (errors={})",
                    reason,
                    backoff,
                    consecutive_errors
                );
                notify!(args.notify, "Round {}: Error — {}, retrying in {}s", iteration, reason, backoff);
                interruptible_sleep(backoff, &running);
                continue;
            }
        }

        // Normal sleep between iterations
        log_debug!("loop: sleeping {}s until next iteration", args.interval);
        interruptible_sleep(args.interval, &running);
    }

    log_info!("loop: stopped after {} iterations", iteration);
    Ok(())
}

enum IterationResult {
    Submitted {
        market: String,
        direction: String,
        tickets: u32,
        tickets_filled: u32,
        order_status: String,  // "filled", "partial", "open"
    },
    Skipped {
        reason: String,
    },
    NoMarkets {
        wait_seconds: u64,
    },
    RateLimited {
        wait_seconds: u64,
    },
    LlmFailed {
        reason: String,
    },
    Error {
        reason: String,
    },
}

fn run_iteration(server_url: &str, openclaw_bin: &str, agent_id: &str) -> IterationResult {
    // 1. Create API client
    let client = match ApiClient::new(server_url.to_string()) {
        Ok(c) => c,
        Err(e) => {
            return IterationResult::Error {
                reason: format!("API client init failed: {e}"),
            }
        }
    };

    // 2. Fetch agent status (includes timeslot, open_orders, recent_results)
    // Auto-refresh wallet token on auth failure
    let status = match client.get_auth("/api/v1/agents/me/status") {
        Ok(v) => v,
        Err(e) => {
            let err_str = e.to_string();
            // Check if this is an auth error that might be fixed by refreshing token
            if err_str.contains("AUTH_FAILED") || err_str.contains("expired") || err_str.contains("invalid token") {
                log_warn!("loop: auth failed, attempting token refresh...");
                match refresh_wallet_token() {
                    Ok(_) => {
                        log_info!("loop: token refreshed, retrying status fetch...");
                        // Recreate client with new token and retry
                        let new_client = match ApiClient::new(server_url.to_string()) {
                            Ok(c) => c,
                            Err(e) => return IterationResult::Error { reason: format!("client reinit failed: {e}") },
                        };
                        match new_client.get_auth("/api/v1/agents/me/status") {
                            Ok(v) => v,
                            Err(e) => return IterationResult::Error { reason: format!("status fetch failed after refresh: {e}") },
                        }
                    }
                    Err(refresh_err) => {
                        log_error!("loop: token refresh failed: {}", refresh_err);
                        return IterationResult::Error {
                            reason: format!("auth failed and token refresh failed: {e} / {refresh_err}"),
                        }
                    }
                }
            } else {
                return IterationResult::Error {
                    reason: format!("status fetch failed: {e}"),
                }
            }
        }
    };
    let agent_data = status.get("data").cloned().unwrap_or(json!({}));
    let balance = agent_data
        .get("balance")
        .and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()))
        .unwrap_or(0.0);
    let persona = agent_data
        .get("persona")
        .and_then(|v| v.as_str())
        .unwrap_or("none");

    // 3. Check timeslot — skip LLM entirely if no submissions remaining
    let timeslot = agent_data.get("timeslot");
    let submissions_remaining = timeslot
        .and_then(|t| t.get("submissions_remaining"))
        .and_then(|v| v.as_i64())
        .unwrap_or(3); // default to 3 if server doesn't return timeslot yet
    let slot_resets_in = timeslot
        .and_then(|t| t.get("slot_resets_in_seconds"))
        .and_then(|v| v.as_u64())
        .unwrap_or(300);
    let submissions_used = timeslot
        .and_then(|t| t.get("submissions_used"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    log_info!(
        "loop: balance={:.0}, persona={}, timeslot={}/{} used, resets in {}s",
        balance, persona, submissions_used,
        timeslot.and_then(|t| t.get("slot_limit")).and_then(|v| v.as_i64()).unwrap_or(3),
        slot_resets_in
    );

    if submissions_remaining <= 0 {
        log_info!("loop: no submissions remaining in this timeslot, waiting {}s for reset", slot_resets_in);
        return IterationResult::RateLimited {
            wait_seconds: slot_resets_in.max(10),
        };
    }

    // Extract open_orders and recent_results for LLM context
    let open_orders = agent_data.get("open_orders").and_then(|v| v.as_array()).cloned();
    let recent_results = agent_data.get("recent_results").and_then(|v| v.as_array()).cloned();

    // 4. Fetch smart market recommendations from server
    let recommendations = match client.get_auth("/api/v1/markets/recommend") {
        Ok(v) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
        Err(e) => {
            log_warn!("loop: recommend endpoint failed ({}), falling back to active markets", e);
            Vec::new()
        }
    };

    // Filter to actionable recommendations (action != "skip", >120s remaining)
    let actionable: Vec<&Value> = recommendations
        .iter()
        .filter(|r| {
            let not_skip = r.get("action").and_then(|a| a.as_str()) != Some("skip");
            let enough_time = r.get("seconds_to_close")
                .and_then(|v| v.as_i64())
                .map(|s| s > 120)
                .unwrap_or(false);
            not_skip && enough_time
        })
        .collect();

    // If no recommendations, fall back to active markets
    let (market_id, market_info) = if !actionable.is_empty() {
        let top = actionable[0];
        let id = top.get("market_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        log_info!(
            "loop: server recommends {} (score={}, reason={})",
            id,
            top.get("score").and_then(|v| v.as_i64()).unwrap_or(0),
            top.get("reason").and_then(|v| v.as_str()).unwrap_or("?")
        );
        (id, top.clone())
    } else {
        // Fallback: fetch active markets and pick first submittable
        log_debug!("loop: no server recommendations, falling back to active markets");
        let markets_resp = match client.get("/api/v1/markets/active") {
            Ok(v) => v,
            Err(e) => {
                return IterationResult::Error {
                    reason: format!("markets fetch failed: {e}"),
                }
            }
        };
        let markets = markets_resp
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if markets.is_empty() {
            return IterationResult::NoMarkets { wait_seconds: 300 };
        }

        let now = chrono::Utc::now();
        let first = markets.iter().find(|m| {
            let close_at = m.get("close_at")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
            close_at.map(|c| (c - now).num_seconds() > 120).unwrap_or(false)
        });
        match first {
            Some(m) => {
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                (id, m.clone())
            }
            None => return IterationResult::NoMarkets { wait_seconds: 300 },
        }
    };

    if market_id.is_empty() {
        return IterationResult::NoMarkets { wait_seconds: 300 };
    }

    // 5. Fetch klines for the chosen market
    let klines_data = client
        .get(&format!("/api/v1/markets/{}/klines", market_id))
        .ok()
        .and_then(|resp| {
            resp.get("data")
                .and_then(|d| d.get("klines"))
                .and_then(|k| k.as_array())
                .cloned()
        });

    let kline_count = klines_data.as_ref().map(|k| k.len()).unwrap_or(0);
    log_info!("loop: target={}, klines={} candles", market_id, kline_count);

    // 6. Build LLM prompt with full context
    let prompt = build_prompt(
        &market_id,
        &market_info,
        &klines_data,
        &recommendations,
        balance,
        persona,
        submissions_remaining,
        slot_resets_in,
        &open_orders,
        &recent_results,
    );

    // 8. Call LLM via OpenClaw
    log_info!("loop: calling LLM via openclaw agent {}...", agent_id);
    let llm_start = Instant::now();
    let llm_response = call_openclaw(openclaw_bin, agent_id, &prompt);
    let llm_elapsed = llm_start.elapsed();

    let llm_text = match llm_response {
        Ok(text) => {
            log_info!("loop: LLM responded ({:.1}s, {} chars)", llm_elapsed.as_secs_f64(), text.len());
            log_debug!("loop: LLM raw output: {}", truncate_str(&text, 500));
            text
        }
        Err(e) => {
            return IterationResult::LlmFailed {
                reason: format!("{e}"),
            }
        }
    };

    // 9. Parse LLM response
    let decision = match parse_llm_response(&llm_text) {
        Ok(parsed) => parsed,
        Err(e) => {
            log_warn!("loop: failed to parse LLM response: {}", e);
            return IterationResult::LlmFailed {
                reason: format!("parse failed: {e}"),
            };
        }
    };

    // Handle skip decision
    let (direction, reasoning, tickets, target_market, limit_price) = match decision {
        LlmDecision::Skip { reason } => {
            log_info!("loop: LLM chose to skip: {}", reason);
            return IterationResult::Skipped { reason };
        }
        LlmDecision::Submit { direction, reasoning, tickets, market_id, limit_price } => {
            (direction, reasoning, tickets, market_id, limit_price)
        }
    };

    // Use target market from LLM if provided, otherwise use recommended
    let final_market = if let Some(ref tm) = target_market {
        if recommendations.iter().any(|m| {
            m.get("market_id").and_then(|v| v.as_str()) == Some(tm.as_str())
                || m.get("id").and_then(|v| v.as_str()) == Some(tm.as_str())
        }) {
            tm.clone()
        } else {
            log_warn!("loop: LLM suggested market {} not in available list, using {}", tm, market_id);
            market_id.clone()
        }
    } else {
        market_id.clone()
    };

    const MIN_TICKETS: u32 = 100;
    let final_tickets = tickets.unwrap_or_else(|| {
        // Default: ~10% of balance, minimum 100
        let t = (balance * 0.10).floor() as u32;
        t.max(MIN_TICKETS)
    });

    // Enforce minimum
    let final_tickets = final_tickets.max(MIN_TICKETS);

    log_info!(
        "loop: submitting {} {} tickets for {} @ {:?}",
        direction,
        final_tickets,
        final_market,
        limit_price
    );

    // 10. Submit prediction
    // Build canonical body for signature (matches server's format)
    // Format: market_id|prediction|limit_price_or_none|tickets|sha256(reasoning)
    let reasoning_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(reasoning.as_bytes()))
    };
    let limit_price_str = limit_price
        .map(|p| format!("{}", p))
        .unwrap_or_else(|| "none".to_string());
    let canonical_body = format!(
        "{}|{}|{}|{}|{}",
        final_market, direction, limit_price_str, final_tickets, reasoning_hash
    );
    log_debug!("loop: canonical body = {}", canonical_body);

    let mut body = json!({
        "market_id": final_market,
        "prediction": direction,
        "tickets": final_tickets,
        "reasoning": reasoning,
    });
    if let Some(lp) = limit_price {
        body["limit_price"] = json!(lp);
    }

    match client.post_auth_with_canonical(canonical_body.as_bytes(), "/api/v1/predictions", &body) {
        Ok(resp) => {
            let data = resp.get("data").cloned().unwrap_or(json!({}));
            let order_status = data
                .get("order_status")
                .and_then(|v| v.as_str())
                .unwrap_or("open")
                .to_string();
            let tickets_filled = data
                .get("tickets_filled")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            log_info!(
                "loop: submission result — status={}, filled={}/{}",
                order_status, tickets_filled, final_tickets
            );
            IterationResult::Submitted {
                market: final_market,
                direction,
                tickets: final_tickets,
                tickets_filled,
                order_status,
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("RATE_LIMIT") || err_str.contains("429") {
                return IterationResult::RateLimited { wait_seconds: 300 };
            }
            if err_str.contains("INSUFFICIENT_BALANCE") {
                log_warn!("loop: insufficient balance, waiting for chip feed");
                return IterationResult::NoMarkets { wait_seconds: 600 };
            }
            IterationResult::Error {
                reason: format!("submit failed: {}", extract_short_error(&err_str)),
            }
        }
    }
}

fn build_prompt(
    market_id: &str,
    recommended: &Value,
    klines: &Option<Vec<Value>>,
    all_markets: &[Value],
    balance: f64,
    persona: &str,
    submissions_remaining: i64,
    slot_resets_in: u64,
    open_orders: &Option<Vec<Value>>,
    recent_results: &Option<Vec<Value>>,
) -> String {
    // Extract market info — support both direct market object and recommend response format
    let asset = recommended.get("asset").and_then(|v| v.as_str()).unwrap_or("BTC/USDT");
    let window = recommended.get("window").and_then(|v| v.as_str()).unwrap_or("15m");
    let implied_up = recommended.get("implied_up_prob")
        .or_else(|| recommended.get("orderbook").and_then(|o| o.get("implied_up_prob")))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let closes_in = recommended.get("seconds_to_close")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            // Fallback: calculate from close_at if seconds_to_close not present
            recommended.get("close_at")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .map(|c| (c - chrono::Utc::now()).num_seconds().max(0))
        })
        .unwrap_or(0);

    let mut prompt = String::with_capacity(6000);

    // Identity, stakes, and motivation
    prompt.push_str(&format!(
        "You are a prediction agent competing in AWP Predict WorkNet{}.\n\n",
        if persona != "none" { format!(" (persona: {})", persona) } else { String::new() }
    ));

    // Persona-specific ticket sizing guidance
    match persona {
        "degen" => {
            prompt.push_str("**Your style (degen):** Go big or go home. Minimum 30% of balance per trade, up to 50% when you see any reasonable signal. Never skip — always find a trade. You're here to make moves, not to watch from the sidelines.\n\n");
        }
        "sniper" => {
            prompt.push_str("**Your style (sniper):** Quality over quantity. When you DO submit, commit 25-40% of balance. If the signal isn't crystal clear, skip and wait for a better setup. But when you pull the trigger, make it count.\n\n");
        }
        "conservative" => {
            prompt.push_str("**Your style (conservative):** Capital preservation first. Keep positions at 5-15% of balance. Only trade on strong, clear signals. It's fine to skip rounds when uncertain.\n\n");
        }
        "contrarian" => {
            prompt.push_str("**Your style (contrarian):** Fade the crowd. When implied_up_prob is extreme (>0.80 or <0.20), bet the opposite direction with 20-35% of balance. The crowd is often wrong at extremes.\n\n");
        }
        _ => {}
    }
    prompt.push_str("## Why This Matters\n\n");
    prompt.push_str("Your predictions are recorded permanently on-chain. Every agent can see your track record — your accuracy rate, your win/loss history, your reasoning quality. Top-performing agents earn significantly more $PRED rewards and build reputation that compounds over time. Poor performers fall behind and become irrelevant.\n\n");
    prompt.push_str("You are competing against other AI agents who are analyzing the same data. The ones who win consistently are not the ones who predict the most — they are the ones who think the hardest about WHEN to commit big and when to stay small. A single well-reasoned contrarian call that hits is worth more than dozens of lazy consensus-following submissions.\n\n");
    prompt.push_str("Treat every prediction as if your track record depends on it — because it does.\n\n");

    // Game rules — the agent must understand the full picture
    prompt.push_str("## Game Rules\n\n");
    prompt.push_str("You are playing a prediction market game against other AI agents. This is a **repeated game** — you will play hundreds of rounds over days and weeks. Your goal is to **maximize your chip balance over time**, not to win any single prediction.\n\n");

    prompt.push_str("**The long game:**\n");
    prompt.push_str("- A single prediction does not matter. What matters is your cumulative P&L across all predictions.\n");
    prompt.push_str("- Winning 6 out of 10 predictions at fair odds (0.50) makes you profitable. Winning 9 out of 10 at terrible odds (0.95) makes you break even.\n");
    prompt.push_str("- The best agents are not the ones who predict the most, or even the most accurately — they are the ones who **size their bets according to their edge**. Big when confident, small when uncertain, zero when the odds are against them.\n");
    prompt.push_str("- Patience is a strategy. Skipping a bad opportunity is as valuable as taking a good one.\n\n");

    prompt.push_str("**How markets work:**\n");
    prompt.push_str("- Each market asks: will this asset's price go UP or DOWN within a time window (15m/30m/1h)?\n");
    prompt.push_str("- You commit chips (virtual tokens) to your prediction. Winners get 1 chip per ticket. Losers get 0.\n");
    prompt.push_str("- Chips come from Chip Feed: 10,000 chips every 4 hours. Your current balance is all you have until the next feed.\n\n");

    prompt.push_str("**How pricing works (CLOB):**\n");
    prompt.push_str("- `implied_up_prob` is the market price, NOT a forecast. It reflects what other agents have already committed.\n");
    prompt.push_str("- When you buy UP at price 0.70, you pay 0.70 chips per ticket. If UP wins, you get 1.00 back (profit 0.30). If DOWN wins, you lose 0.70.\n");
    prompt.push_str("- When you buy DOWN at price 0.70 (meaning implied_up=0.70), you pay 0.30 per ticket. If DOWN wins, you get 1.00 (profit 0.70). If UP wins, you lose 0.30.\n");
    prompt.push_str("- **The price IS your breakeven accuracy.** At 0.70 UP, you need >70% accuracy on UP calls to profit. If your true edge is only 60%, buying UP at 0.70 is a losing play even if UP wins this time.\n\n");

    prompt.push_str("**Using limit_price to express conviction:**\n");
    prompt.push_str("- If implied_up_prob is 0.50 and you think UP has 65% true probability, bid 0.55-0.60 for UP. You're paying less than your expected value.\n");
    prompt.push_str("- If you think UP has 80% probability, you can bid up to 0.75 and still have edge.\n");
    prompt.push_str("- DO NOT just bid 0.50 every time. That's leaving money on the table. Express your conviction in the price!\n");
    prompt.push_str("- Higher bids fill faster but have lower profit margin. Lower bids have higher margin but may not fill.\n\n");

    prompt.push_str("**How you earn $PRED rewards:**\n");
    prompt.push_str("- Participation Pool (20%): proportional to your submission count (capped at 300/day).\n");
    prompt.push_str("- Alpha Pool (80%): proportional to your excess_score = max(0, balance - total_chips_fed_today). You earn Alpha only if you **grew** your chip balance beyond what was given.\n");
    prompt.push_str("- The Alpha Pool is where the real money is. One well-sized winning prediction can earn more Alpha than dozens of small break-even ones.\n\n");

    prompt.push_str("**Constraints and timing:**\n");
    prompt.push_str("- **3 submissions per 15-minute timeslot. USE ALL 3.** Each submission earns participation rewards.\n");
    prompt.push_str("- Unused submissions = wasted $PRED. Don't leave slots on the table.\n");
    prompt.push_str("- You can spread them out or batch them, but by the end of the timeslot you should have submitted 3 times.\n");
    prompt.push_str("- You can choose ANY market from the available list, not just the recommended one.\n\n");

    // Response format
    prompt.push_str("## Your Response\n\n");
    prompt.push_str("Output a JSON object with these fields:\n");
    prompt.push_str("- \"action\": \"submit\" or \"skip\" — whether to place a prediction this round\n");
    prompt.push_str("- \"direction\": \"up\" or \"down\" — your prediction (required if action=submit)\n");
    prompt.push_str("- \"reasoning\": your analysis (80-2000 chars, ≥2 sentences, must mention the asset or a direction word). Required if action=submit. If skipping, briefly explain why.\n");
    prompt.push_str(&format!("- \"tickets\": how many chips to commit (integer, minimum 100, max {:.0}). Size according to your persona and conviction!\n", balance));
    prompt.push_str(&format!("- \"market_id\": which market (default: \"{}\", required if action=submit)\n", market_id));
    prompt.push_str("- \"limit_price\": (optional, 0.01-0.99) the max price you're willing to pay. If you believe UP has 70% probability, bid 0.60-0.65 to get edge. Higher price = easier fill but less profit. Omit for market order.\n\n");
    prompt.push_str("**Skipping is rarely correct.** You should submit 3 times per timeslot. Only skip if:\n");
    prompt.push_str("- All markets closing in <60 seconds (no time to fill)\n");
    prompt.push_str("- You already used all 3 submissions this timeslot\n\n");
    prompt.push_str("If you have submissions remaining, FIND A TRADE. Pick the best market and submit.\n\n");
    prompt.push_str("## Research (Optional)\n\n");
    prompt.push_str("If you have tools available, you may research before deciding:\n");
    prompt.push_str("- Search for recent news about the asset\n");
    prompt.push_str("- Check market sentiment\n");
    prompt.push_str("- Look up relevant data\n\n");
    prompt.push_str("Better analysis = better decisions. Take time if it helps.\n\n");
    prompt.push_str("## Final Output\n\n");
    prompt.push_str("Output your decision on a line starting with `DECISION:` followed by a JSON object:\n\n");
    prompt.push_str("```\n");
    prompt.push_str("DECISION: {\"action\": \"submit\", \"direction\": \"up\", \"tickets\": 3000, \"market_id\": \"...\", \"limit_price\": 0.55, \"reasoning\": \"...\"}\n");
    prompt.push_str("```\n\n");
    prompt.push_str("Required fields:\n");
    prompt.push_str("- \"action\": \"submit\" or \"skip\"\n");
    prompt.push_str("- \"direction\": \"up\" or \"down\" (if submitting)\n");
    prompt.push_str("- \"reasoning\": 80-2000 chars, ≥2 sentences, must mention the asset or a direction word\n");
    prompt.push_str(&format!("- \"tickets\": integer, minimum 100, max {:.0}\n", balance));
    prompt.push_str(&format!("- \"market_id\": which market (default: \"{}\")\n", market_id));
    prompt.push_str("- \"limit_price\": (optional, 0.01-0.99) your bid price\n\n");
    prompt.push_str("**All text must be in English.**\n\n");

    // Current state with timeslot
    prompt.push_str("## Your Current State\n\n");
    prompt.push_str(&format!("- Balance: {:.0} chips\n", balance));

    // Persona-specific sizing with concrete numbers
    let (min_pct, max_pct, sizing_note) = match persona {
        "degen" => (0.30, 0.50, "GO BIG. 400 tickets is NOT degen behavior."),
        "sniper" => (0.25, 0.40, "When you shoot, make it count."),
        "conservative" => (0.05, 0.15, "Stay disciplined."),
        "contrarian" => (0.20, 0.35, "Fade the crowd with conviction."),
        _ => (0.15, 0.25, "Size according to conviction."),
    };
    let min_tickets = (balance * min_pct).floor() as u32;
    let max_tickets = (balance * max_pct).floor() as u32;
    prompt.push_str(&format!(
        "- **Your sizing ({}):** {}-{} tickets per trade. {}\n",
        persona, min_tickets, max_tickets, sizing_note
    ));

    // Submissions remaining with urgency
    if submissions_remaining > 0 {
        prompt.push_str(&format!("- **Submissions remaining: {}/3** — ", submissions_remaining));
        if submissions_remaining == 3 {
            prompt.push_str("use all 3 before timeslot ends!\n");
        } else if submissions_remaining == 2 {
            prompt.push_str("2 left, keep submitting!\n");
        } else {
            prompt.push_str("last chance this timeslot!\n");
        }
    } else {
        prompt.push_str("- Submissions: 0/3 remaining — wait for next timeslot\n");
    }

    if slot_resets_in > 0 {
        let mins_left = slot_resets_in / 60;
        let secs_left = slot_resets_in % 60;
        if mins_left > 10 {
            prompt.push_str(&format!("- Timeslot resets in {}m\n", mins_left));
        } else if mins_left > 3 {
            prompt.push_str(&format!("- Timeslot resets in {}m{}s\n", mins_left, secs_left));
        } else if submissions_remaining > 0 {
            prompt.push_str(&format!("- **URGENT: {}m{}s left! Submit NOW or lose {} slot(s)!**\n", mins_left, secs_left, submissions_remaining));
        } else {
            prompt.push_str(&format!("- Timeslot resets in {}m{}s\n", mins_left, secs_left));
        }
    }
    prompt.push_str(&format!("- Available markets: {}\n", all_markets.len()));

    // Open positions with fill status and anti-contradiction warning
    if let Some(orders) = open_orders {
        if !orders.is_empty() {
            // Calculate fill statistics
            let mut total_tickets: i64 = 0;
            let mut total_filled: i64 = 0;
            for o in orders.iter() {
                total_tickets += o.get("tickets").and_then(|v| v.as_i64()).unwrap_or(0);
                total_filled += o.get("tickets_filled").and_then(|v| v.as_i64()).unwrap_or(0);
            }
            let fill_rate = if total_tickets > 0 { (total_filled as f64 / total_tickets as f64 * 100.0) as i64 } else { 0 };

            prompt.push_str(&format!(
                "\n**Your open orders ({}, fill rate: {}%)**\n",
                orders.len(),
                fill_rate
            ));
            for o in orders.iter().take(10) {
                let tickets = o.get("tickets").and_then(|v| v.as_i64()).unwrap_or(0);
                let filled = o.get("tickets_filled").and_then(|v| v.as_i64()).unwrap_or(0);
                let status = if filled == tickets {
                    "FILLED"
                } else if filled > 0 {
                    "PARTIAL"
                } else {
                    "PENDING"
                };
                prompt.push_str(&format!(
                    "- {} {} {} — {} {}/{} tickets, closes {}\n",
                    o.get("asset").and_then(|v| v.as_str()).unwrap_or("?"),
                    o.get("window").and_then(|v| v.as_str()).unwrap_or("?"),
                    o.get("direction").and_then(|v| v.as_str()).unwrap_or("?").to_uppercase(),
                    status,
                    filled,
                    tickets,
                    o.get("close_at").and_then(|v| v.as_str()).unwrap_or("?"),
                ));
            }
            prompt.push_str("\n**Understanding fill status:**\n");
            prompt.push_str("- FILLED: Your chips are matched. You have real exposure and will win/lose at settlement.\n");
            prompt.push_str("- PARTIAL: Some matched, rest waiting. Unmatched portion refunds at market close.\n");
            prompt.push_str("- PENDING: No matches yet. Chips are locked but you have no actual exposure until matched.\n\n");
            prompt.push_str("**CRITICAL: Do NOT bet against your open positions.**\n");
            prompt.push_str("Betting both UP and DOWN on the same market guarantees a loss.\n\n");
        }
    }

    // Recent results
    if let Some(results) = recent_results {
        if !results.is_empty() {
            let wins = results.iter().filter(|r| r.get("won").and_then(|v| v.as_bool()).unwrap_or(false)).count();
            prompt.push_str(&format!(
                "\n**Recent results (last {}, {} wins):**\n",
                results.len(),
                wins
            ));
            for r in results.iter().take(5) {
                let won = r.get("won").and_then(|v| v.as_bool()).unwrap_or(false);
                prompt.push_str(&format!(
                    "- {} {} {} — {} (payout: {}, spent: {})\n",
                    r.get("asset").and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("window").and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("direction").and_then(|v| v.as_str()).unwrap_or("?").to_uppercase(),
                    if won { "WON" } else { "LOST" },
                    r.get("payout_chips").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("chips_spent").and_then(|v| v.as_i64()).unwrap_or(0),
                ));
            }
        }
    }
    prompt.push('\n');

    // Recommended market
    prompt.push_str("## Recommended Market\n\n");
    prompt.push_str(&format!("- ID: {}\n", market_id));
    prompt.push_str(&format!("- Asset: {}\n", asset));
    prompt.push_str(&format!("- Window: {}\n", window));
    prompt.push_str(&format!("- Closes in: {}s\n", closes_in));
    prompt.push_str(&format!("- implied_up_prob: {:.2}\n", implied_up));
    // Server recommendation context
    if let Some(reason) = recommended.get("reason").and_then(|v| v.as_str()) {
        prompt.push_str(&format!("- Server insight: {}\n", reason));
    }
    if let Some(suggested) = recommended.get("suggested_side").and_then(|v| v.as_str()) {
        if suggested != "skip" {
            prompt.push_str(&format!("- Liquidity favors: {} (counterparty orders waiting)\n", suggested.to_uppercase()));
        }
    }
    // Orderbook detail with best prices
    if let Some(ob) = recommended.get("orderbook") {
        // Best prices and spread
        let best_up = ob.get("best_up_price").and_then(|v| v.as_str());
        let best_down = ob.get("best_down_price").and_then(|v| v.as_str());
        let spread = ob.get("spread").and_then(|v| v.as_f64());

        if best_up.is_some() || best_down.is_some() {
            prompt.push_str("- **Best prices available:**\n");
            if let Some(up_price) = best_up {
                prompt.push_str(&format!("  - Buy UP: {} (bid at this price to get filled)\n", up_price));
            } else {
                prompt.push_str("  - Buy UP: no orders (your order will wait for counterparty)\n");
            }
            if let Some(down_price) = best_down {
                prompt.push_str(&format!("  - Buy DOWN: {} (bid at this price to get filled)\n", down_price));
            } else {
                prompt.push_str("  - Buy DOWN: no orders (your order will wait for counterparty)\n");
            }
            if let Some(s) = spread {
                if s > 0.1 {
                    prompt.push_str(&format!("  - Spread: {:.2} (WIDE — consider placing limit orders)\n", s));
                } else if s > 0.05 {
                    prompt.push_str(&format!("  - Spread: {:.2} (moderate)\n", s));
                } else {
                    prompt.push_str(&format!("  - Spread: {:.2} (tight — good liquidity)\n", s));
                }
            }
        }

        prompt.push_str(&format!(
            "- Volume: UP filled={} open={}, DOWN filled={} open={}\n",
            ob.get("up_filled").and_then(|v| v.as_i64()).unwrap_or(0),
            ob.get("up_open").and_then(|v| v.as_i64()).unwrap_or(0),
            ob.get("down_filled").and_then(|v| v.as_i64()).unwrap_or(0),
            ob.get("down_open").and_then(|v| v.as_i64()).unwrap_or(0),
        ));
    }
    // Last prediction on this asset — enables continuity
    if let Some(lp) = recommended.get("last_prediction") {
        if !lp.is_null() {
            let lp_dir = lp.get("direction").and_then(|v| v.as_str()).unwrap_or("?");
            let lp_won = lp.get("won").and_then(|v| v.as_bool());
            let lp_outcome = lp.get("outcome").and_then(|v| v.as_str()).unwrap_or("pending");
            let lp_reasoning = lp.get("reasoning_text").and_then(|v| v.as_str()).unwrap_or("");
            prompt.push_str(&format!(
                "\n**Your last prediction on {}:**\n",
                asset
            ));
            prompt.push_str(&format!("- Direction: {}\n", lp_dir.to_uppercase()));
            match lp_won {
                Some(true) => prompt.push_str(&format!("- Result: WON (outcome was {})\n", lp_outcome)),
                Some(false) => prompt.push_str(&format!("- Result: LOST (outcome was {})\n", lp_outcome)),
                None => prompt.push_str("- Result: pending (market not yet resolved)\n"),
            }
            if !lp_reasoning.is_empty() {
                prompt.push_str(&format!("- Your reasoning was: \"{}\"\n", lp_reasoning));
            }
            prompt.push_str("- Consider: was your thesis correct? Should you continue or reverse?\n");
        }
    }
    // Explain the odds concretely
    if implied_up > 0.5 {
        prompt.push_str(&format!(
            "  → Buying UP costs {:.2}, profit if correct: {:.2}. Buying DOWN costs {:.2}, profit if correct: {:.2}.\n",
            implied_up, 1.0 - implied_up, 1.0 - implied_up, implied_up
        ));
    } else if implied_up < 0.5 {
        prompt.push_str(&format!(
            "  → Buying UP costs {:.2}, profit if correct: {:.2}. Buying DOWN costs {:.2}, profit if correct: {:.2}.\n",
            implied_up, 1.0 - implied_up, 1.0 - implied_up, implied_up
        ));
    } else {
        prompt.push_str("  → Fair odds (0.50/0.50). Your edge comes purely from analysis.\n");
    }
    prompt.push('\n');

    // Klines data
    if let Some(candles) = klines {
        if !candles.is_empty() {
            prompt.push_str(&format!("## Klines ({} candles)\n\n", candles.len()));
            prompt.push_str("time | open | high | low | close | volume\n");
            prompt.push_str("--- | --- | --- | --- | --- | ---\n");
            let start = if candles.len() > 20 { candles.len() - 20 } else { 0 };
            for candle in &candles[start..] {
                if let Some(obj) = candle.as_object() {
                    prompt.push_str(&format!(
                        "{} | {} | {} | {} | {} | {}\n",
                        obj.get("open_time").and_then(|v| v.as_i64()).unwrap_or(0),
                        obj.get("open").and_then(|v| v.as_f64()).map(|f| format!("{:.2}", f)).unwrap_or_default(),
                        obj.get("high").and_then(|v| v.as_f64()).map(|f| format!("{:.2}", f)).unwrap_or_default(),
                        obj.get("low").and_then(|v| v.as_f64()).map(|f| format!("{:.2}", f)).unwrap_or_default(),
                        obj.get("close").and_then(|v| v.as_f64()).map(|f| format!("{:.2}", f)).unwrap_or_default(),
                        obj.get("volume").and_then(|v| v.as_f64()).map(|f| format!("{:.0}", f)).unwrap_or_default(),
                    ));
                }
            }
            prompt.push('\n');
        } else {
            prompt.push_str("## Klines\n\nNo kline data available. Use market data and general market awareness.\n\n");
        }
    } else {
        prompt.push_str("## Klines\n\nNo kline data available. Use market data and general market awareness.\n\n");
    }

    // Other available markets from server recommendations
    if all_markets.len() > 1 {
        prompt.push_str("## Other Markets (server-ranked)\n\n");
        for m in all_markets.iter().skip(1).take(8) {
            let reason = m.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            let suggested = m.get("suggested_side").and_then(|v| v.as_str()).unwrap_or("?");
            let score = m.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
            let mid = m.get("market_id").or_else(|| m.get("id")).and_then(|v| v.as_str()).unwrap_or("?");
            let masset = m.get("asset").and_then(|v| v.as_str()).unwrap_or("?");
            let mwindow = m.get("window").and_then(|v| v.as_str()).unwrap_or("?");
            // Include last prediction summary if available
            let lp_hint = m.get("last_prediction")
                .filter(|lp| !lp.is_null())
                .and_then(|lp| {
                    let dir = lp.get("direction").and_then(|v| v.as_str())?;
                    let result = match lp.get("won").and_then(|v| v.as_bool()) {
                        Some(true) => "won",
                        Some(false) => "lost",
                        None => "pending",
                    };
                    Some(format!(" [last: {} {}]", dir, result))
                })
                .unwrap_or_default();
            prompt.push_str(&format!(
                "- {} ({} {}) score={} suggested={}{} — {}\n",
                mid, masset, mwindow, score, suggested, lp_hint, reason
            ));
        }
        prompt.push_str("\nYou may choose a different market by setting \"market_id\" in your response.\n\n");
    }

    prompt
}

fn call_openclaw(openclaw_bin: &str, agent_id: &str, prompt: &str) -> Result<String> {
    // Purge sessions before calling to prevent context overflow
    let _ = Command::new(openclaw_bin)
        .args(["sessions", "purge", "--agent", agent_id, "--yes"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Write prompt to temp file to avoid shell escaping issues
    let tmp_path = std::env::temp_dir().join(format!("predict-prompt-{}.txt", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp_path)
            .context("failed to create temp prompt file")?;
        f.write_all(prompt.as_bytes())?;
    }

    // Read prompt from file and pipe to openclaw
    let prompt_content = std::fs::read_to_string(&tmp_path)?;

    // Use --thinking high for deeper reasoning before deciding
    // The agent can still search web, use tools via the gateway
    // --timeout 180 gives enough time for research (default is 600)
    let output = Command::new(openclaw_bin)
        .args([
            "agent",
            "--agent", agent_id,
            "--message", &prompt_content,
            "--thinking", "high",
            "--timeout", "180",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context(format!("failed to execute openclaw at {}", openclaw_bin))?;

    // Clean up temp file
    let _ = std::fs::remove_file(&tmp_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        // Check for rate limiting
        if stderr.contains("rate limit") || stderr.contains("429") {
            anyhow::bail!("OpenClaw rate limited (exit {}): {}", code, stderr.trim());
        }
        anyhow::bail!("openclaw failed (exit {}): {}", code, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        anyhow::bail!("openclaw returned empty response");
    }
    Ok(stdout)
}

/// Parsed LLM response — either a submission or a skip
enum LlmDecision {
    Submit {
        direction: String,
        reasoning: String,
        tickets: Option<u32>,
        market_id: Option<String>,
        limit_price: Option<f64>,
    },
    Skip {
        reason: String,
    },
}

fn parse_llm_response(text: &str) -> Result<LlmDecision> {
    // Try to extract JSON from the response
    // LLMs sometimes wrap JSON in markdown fences or add text around it
    let json_str = extract_json(text)
        .context("no JSON object found in LLM response")?;

    let v: Value = serde_json::from_str(&json_str)
        .context(format!("invalid JSON from LLM: {}", truncate_str(&json_str, 200)))?;

    // Check for skip action
    let action = v
        .get("action")
        .and_then(|a| a.as_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "submit".to_string()); // default to submit for backwards compat

    if action == "skip" {
        let reason = v
            .get("reasoning")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "No reason provided".to_string());
        return Ok(LlmDecision::Skip { reason });
    }

    // Parse submit action
    let direction = v
        .get("direction")
        .and_then(|d| d.as_str())
        .map(|s| s.to_lowercase())
        .filter(|s| s == "up" || s == "down")
        .context("missing or invalid 'direction' (must be 'up' or 'down')")?;

    let reasoning = v
        .get("reasoning")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .filter(|s| s.len() >= 80)
        .context("missing or too short 'reasoning' (must be >= 80 chars)")?;

    let tickets = v
        .get("tickets")
        .and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64)))
        .map(|t| t.max(1) as u32);

    let market_id = v
        .get("market_id")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    let limit_price = v
        .get("limit_price")
        .and_then(|p| p.as_f64())
        .filter(|p| *p >= 0.01 && *p <= 0.99);

    Ok(LlmDecision::Submit {
        direction,
        reasoning,
        tickets,
        market_id,
        limit_price,
    })
}

/// Extract JSON object from text that may contain markdown fences or surrounding text.
/// For agentic mode, looks for "DECISION:" prefix first, then falls back to generic JSON extraction.
fn extract_json(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // Priority 1: Look for "DECISION:" prefix (agentic mode output)
    // This handles cases where the agent does research/thinking before outputting the decision
    for prefix in &["DECISION:", "DECISION :", "decision:", "Decision:"] {
        if let Some(pos) = trimmed.find(prefix) {
            let after_prefix = &trimmed[pos + prefix.len()..];
            // Find the JSON object after DECISION:
            if let Some(json_start) = after_prefix.find('{') {
                let json_part = &after_prefix[json_start..];
                // Find matching closing brace
                let mut depth = 0;
                let mut json_end = 0;
                for (i, ch) in json_part.chars().enumerate() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                json_end = i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if json_end > 0 {
                    let candidate = &json_part[..json_end];
                    if serde_json::from_str::<Value>(candidate).is_ok() {
                        return Some(candidate.to_string());
                    }
                }
            }
        }
    }

    // Priority 2: Try parsing the whole thing first
    if trimmed.starts_with('{') {
        if serde_json::from_str::<Value>(trimmed).is_ok() {
            return Some(trimmed.to_string());
        }
    }

    // Priority 3: Try to find JSON inside markdown code fences
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if serde_json::from_str::<Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if candidate.starts_with('{') {
                if serde_json::from_str::<Value>(candidate).is_ok() {
                    return Some(candidate.to_string());
                }
            }
        }
    }

    // Priority 4: Find last JSON object (more likely to be the decision in agentic output)
    // Search from the end of the text
    if let Some(last_close) = trimmed.rfind('}') {
        // Find the matching open brace by counting backwards
        let before_close = &trimmed[..=last_close];
        let mut depth = 0;
        let mut json_start = None;
        for (i, ch) in before_close.chars().rev().enumerate() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    depth -= 1;
                    if depth == 0 {
                        json_start = Some(before_close.len() - 1 - i);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(start) = json_start {
            let candidate = &trimmed[start..=last_close];
            if serde_json::from_str::<Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }

    // Fallback: Find first { and last } and try parsing
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end > start {
        let candidate = &trimmed[start..=end];
        if serde_json::from_str::<Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

fn detect_openclaw() -> Option<String> {
    for name in &["openclaw", "openclaw.mjs", "openclaw.cmd"] {
        if which_exists(name) {
            return Some(name.to_string());
        }
    }
    // Check well-known paths
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/openclaw"),
        format!("{home}/.npm-global/bin/openclaw"),
        "/usr/local/bin/openclaw".to_string(),
    ];
    for path in &candidates {
        if std::path::Path::new(path).is_file() {
            return Some(path.clone());
        }
    }
    None
}

fn which_exists(name: &str) -> bool {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .any(|dir| std::path::Path::new(dir).join(name).is_file())
}

fn ensure_agent(openclaw_bin: &str, agent_id: &str) {
    // Check if agent exists
    let check = Command::new(openclaw_bin)
        .args(["agents", "list"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    if let Ok(output) = check {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains(agent_id) {
            log_debug!("loop: openclaw agent '{}' already exists", agent_id);
            return;
        }
    }

    // Create agent
    log_info!("loop: creating openclaw agent '{}'...", agent_id);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let workspace = format!("{}/.openclaw/workspace-{}", home, agent_id);
    let result = Command::new(openclaw_bin)
        .args([
            "agents",
            "add",
            agent_id,
            "--workspace",
            &workspace,
            "--non-interactive",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();

    match result {
        Ok(status) if status.success() => {
            log_info!("loop: created openclaw agent '{}'", agent_id);
        }
        Ok(status) => {
            log_warn!(
                "loop: openclaw agent create exited with {} (may already exist)",
                status
            );
        }
        Err(e) => {
            log_warn!("loop: failed to create openclaw agent: {}", e);
        }
    }
}

fn calculate_backoff(base: u64, consecutive: u32, server_hint: Option<u64>) -> u64 {
    if let Some(hint) = server_hint {
        return hint;
    }
    // Exponential backoff: base * 2^consecutive, capped at 600s
    let multiplier = 2u64.pow(consecutive.min(4));
    (base * multiplier).min(600)
}

fn interruptible_sleep(seconds: u64, running: &Arc<AtomicBool>) {
    let end = Instant::now() + std::time::Duration::from_secs(seconds);
    while Instant::now() < end && running.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn extract_short_error(err: &str) -> String {
    if let Some(start) = err.find('{') {
        if let Ok(v) = serde_json::from_str::<Value>(&err[start..]) {
            if let Some(msg) = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                return msg.to_string();
            }
        }
    }
    err.chars().take(200).collect()
}

/// Truncate a string to at most `max_chars` characters (not bytes).
/// Safely handles multi-byte UTF-8 characters like →, Chinese, emoji.
fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max_chars).collect::<String>())
    }
}
