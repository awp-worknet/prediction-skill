/// preflight — check all prerequisites before the main loop.
///
/// Checks (in order):
///   1. awp-wallet installed (or AWP_ADDRESS / AWP_PRIVATE_KEY set)
///   2. AWP network registration (auto-register if needed, gasless)
///   3. Coordinator reachable
///   4. Agent status fetchable (auth works)
///
/// Each step logs progress to stderr. On failure, outputs structured JSON
/// with error details, debug info, and _internal.next_command for recovery.
///
/// On first run (no persona set), presents persona choices for user selection.

use anyhow::Result;
use serde_json::json;

use crate::auth::get_address;
use crate::awp_register;
use crate::client::{check_server, ApiClient};
use crate::output::{Choice, Internal, Output};
use crate::wallet::WalletStatus;
use crate::{log_error, log_info};

/// Valid personas with descriptions
/// Risk styles control position sizing and skip behavior
/// Analysis styles control how you interpret market data
const PERSONAS: &[(&str, &str)] = &[
    // Risk styles
    ("degen", "30-50% positions, never skips a round, always finds a trade"),
    ("conservative", "5-10% positions, only strong signals, skip when uncertain"),
    ("sniper", "may skip many rounds, but heavy (20-40%) when confident"),
    ("contrarian", "fade the crowd, bet against extreme implied probabilities"),
    // Analysis styles
    ("chartist", "technical patterns, indicators, support/resistance levels"),
    ("macro", "rates, DXY, equity correlations, risk-on/off flows"),
    ("sentiment", "social media pulse, Fear & Greed, crowded trade detection"),
];

pub fn run(server_url: &str) -> Result<()> {
    log_info!("preflight: starting (server={})", server_url);

    // Step 1: resolve wallet address
    log_info!("preflight [1/4]: resolving wallet address...");
    let address = match get_address() {
        Ok(a) => {
            log_info!("preflight [1/4]: wallet address = {}", a);
            a
        }
        Err(e) => {
            log_error!("preflight [1/4]: wallet resolution failed: {}", e);

            // Use WalletStatus for safe, accurate guidance
            let wallet_status = WalletStatus::check();
            log_info!(
                "preflight [1/4]: wallet check — cli={}, dir={}, keystore={}, can_receive={}",
                wallet_status.cli_installed,
                wallet_status.wallet_dir_exists,
                wallet_status.has_keystore,
                wallet_status.can_receive
            );

            Output::error_with_debug(
                format!("Cannot determine wallet address: {e}"),
                "WALLET_NOT_CONFIGURED",
                "dependency",
                false,
                wallet_status.suggestion(),
                json!({
                    "step": "1_wallet_address",
                    "error_detail": format!("{e}"),
                    "wallet_status": {
                        "cli_installed": wallet_status.cli_installed,
                        "wallet_dir_exists": wallet_status.wallet_dir_exists,
                        "has_keystore": wallet_status.has_keystore,
                        "can_receive": wallet_status.can_receive,
                        "safe_to_init": wallet_status.safe_to_init(),
                        "human_status": wallet_status.human_status,
                    },
                    "env_AWP_ADDRESS": std::env::var("AWP_ADDRESS").is_ok(),
                    "env_AWP_PRIVATE_KEY": std::env::var("AWP_PRIVATE_KEY").is_ok(),
                    "env_AWP_WALLET_TOKEN": std::env::var("AWP_WALLET_TOKEN").is_ok(),
                    "env_AWP_DEV_MODE": std::env::var("AWP_DEV_MODE").ok(),
                }),
                Internal {
                    next_action: "configure_wallet".into(),
                    next_command: Some(wallet_status.setup_command().into()),
                    progress: Some("0/4".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    // Step 2: AWP network registration
    // Skip in dev mode (no real wallet to sign EIP-712 with)
    let is_dev = std::env::var("AWP_DEV_MODE").as_deref() == Ok("true")
        || std::env::var("AWP_DEV_MODE").as_deref() == Ok("1");

    if is_dev {
        log_info!("preflight [2/4]: skipping AWP registration (dev mode)");
    } else {
        log_info!("preflight [2/4]: checking AWP network registration...");
        match awp_register::check_registration(&address) {
            Ok(true) => {
                log_info!("preflight [2/4]: already registered on AWP network");
            }
            Ok(false) => {
                log_info!("preflight [2/4]: not registered, attempting auto-registration...");
                // Try auto-register
                let token = std::env::var("AWP_WALLET_TOKEN").unwrap_or_default();
                if token.is_empty() {
                    log_error!("preflight [2/4]: AWP_WALLET_TOKEN not set, cannot auto-register");
                    Output::error_with_debug(
                        "Not registered on AWP network. Wallet token needed for auto-registration.",
                        "AWP_NOT_REGISTERED",
                        "dependency",
                        false,
                        "Run: export AWP_WALLET_TOKEN=$(awp-wallet unlock --duration 86400 --scope full --raw)",
                        json!({
                            "step": "2_awp_registration",
                            "address": address,
                            "has_wallet_token": false,
                        }),
                        Internal {
                            next_action: "configure_wallet".into(),
                            next_command: Some("export AWP_WALLET_TOKEN=$(awp-wallet unlock --duration 86400 --scope full --raw)".into()),
                            progress: Some("1/4".into()),
                            ..Default::default()
                        },
                    )
                    .print();
                    return Ok(());
                }

                match awp_register::ensure_registered(&address, &token) {
                    Ok(result) if result.registered => {
                        log_info!(
                            "preflight [2/4]: registration OK — {}{}",
                            result.message,
                            if result.auto_registered { " (auto-registered)" } else { "" }
                        );
                    }
                    Ok(result) => {
                        log_error!("preflight [2/4]: registration incomplete: {}", result.message);
                        Output::error_with_debug(
                            format!("AWP registration incomplete: {}", result.message),
                            "AWP_REGISTRATION_PENDING",
                            "dependency",
                            true,
                            "Registration submitted. Wait a moment and retry.",
                            json!({
                                "step": "2_awp_registration",
                                "address": address,
                                "auto_registered": result.auto_registered,
                                "message": result.message,
                            }),
                            Internal {
                                next_action: "retry".into(),
                                wait_seconds: Some(10),
                                next_command: Some("predict-agent preflight".into()),
                                progress: Some("1/4".into()),
                                ..Default::default()
                            },
                        )
                        .print();
                        return Ok(());
                    }
                    Err(e) => {
                        log_error!("preflight [2/4]: registration failed: {}", e);
                        Output::error_with_debug(
                            format!("AWP registration failed: {e}"),
                            "AWP_REGISTRATION_FAILED",
                            "dependency",
                            true,
                            "Check network connectivity to api.awp.sh and retry.",
                            json!({
                                "step": "2_awp_registration",
                                "address": address,
                                "error_detail": format!("{e}"),
                                "error_chain": format!("{e:#}"),
                            }),
                            Internal {
                                next_action: "retry".into(),
                                wait_seconds: Some(30),
                                next_command: Some("predict-agent preflight".into()),
                                progress: Some("1/4".into()),
                                ..Default::default()
                            },
                        )
                        .print();
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                // AWP API unreachable — don't block, just warn
                log_info!(
                    "preflight [2/4]: AWP API unreachable ({}), skipping registration check",
                    e
                );
            }
        }
    }

    // Step 3: coordinator reachable
    log_info!("preflight [3/4]: checking coordinator connectivity...");
    if let Err(e) = check_server(server_url) {
        log_error!("preflight [3/4]: coordinator unreachable: {}", e);
        Output::error_with_debug(
            format!("Cannot reach coordinator at {server_url}: {e}"),
            "COORDINATOR_UNREACHABLE",
            "network",
            true,
            format!("Check PREDICT_SERVER_URL and network. Tried: {server_url}"),
            json!({
                "step": "3_coordinator_check",
                "server_url": server_url,
                "error_detail": format!("{e}"),
                "error_chain": format!("{e:#}"),
            }),
            Internal {
                next_action: "retry".into(),
                wait_seconds: Some(30),
                next_command: Some("predict-agent preflight".into()),
                progress: Some("2/4".into()),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }
    log_info!("preflight [3/4]: coordinator reachable at {}", server_url);

    // Step 4: fetch agent status (auth verification)
    log_info!("preflight [4/4]: verifying auth (fetching agent status)...");
    let client = ApiClient::new(server_url.to_string())?;
    let status = match client.get_auth("/api/v1/agents/me/status") {
        Ok(v) => {
            log_info!("preflight [4/4]: auth verified, agent status fetched");
            v
        }
        Err(e) => {
            log_error!("preflight [4/4]: auth failed: {}", e);
            let wallet_id = std::env::var("AWP_SESSION_ID")
                .or_else(|_| std::env::var("AWP_AGENT_ID"))
                .unwrap_or_else(|_| "default".to_string());
            let hint = if e.to_string().contains("Wallet address mismatch") {
                "AWP_AGENT_ID or AWP_SESSION_ID may have changed. Try: unset AWP_AGENT_ID AWP_SESSION_ID"
            } else {
                "Check your wallet configuration and ensure the timestamp is fresh."
            };
            Output::error_with_debug(
                format!("Failed to fetch agent status: {e}"),
                "AUTH_FAILED",
                "auth",
                false,
                hint,
                json!({
                    "step": "4_auth_check",
                    "address": address,
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                    "error_chain": format!("{e:#}"),
                    "signing_mode": if std::env::var("AWP_PRIVATE_KEY").is_ok() { "private_key" }
                        else if is_dev { "dev_mode" }
                        else { "awp_wallet" },
                    "wallet_id": wallet_id,
                    "env_AWP_SESSION_ID": std::env::var("AWP_SESSION_ID").ok(),
                    "env_AWP_AGENT_ID": std::env::var("AWP_AGENT_ID").ok(),
                }),
                Internal {
                    next_action: "configure_wallet".into(),
                    next_command: Some("predict-agent preflight".into()),
                    progress: Some("3/4".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = status.get("data").cloned().unwrap_or(json!({}));
    let total_predictions = data
        .get("total_predictions")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let balance_raw = data.get("balance").and_then(|v| v.as_str()).unwrap_or("0");
    let balance = balance_raw
        .parse::<f64>()
        .map(|n| format!("{:.2}", n))
        .unwrap_or_else(|_| balance_raw.to_string());

    // Fetch open market count
    let open_markets = match client.get("/api/v1/markets/active") {
        Ok(v) => v
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        Err(e) => {
            log_info!("preflight: could not fetch active markets count: {}", e);
            0
        }
    };

    // Extract persona from status
    let persona = data
        .get("persona")
        .and_then(|v| v.as_str())
        .unwrap_or("none");

    log_info!(
        "preflight: READY — {} open markets, {} total predictions, {} chips, persona={}",
        open_markets,
        total_predictions,
        balance,
        persona
    );

    // Capture wallet isolation context for debugging
    let wallet_id = std::env::var("AWP_SESSION_ID")
        .or_else(|_| std::env::var("AWP_AGENT_ID"))
        .unwrap_or_else(|_| "default".to_string());

    // Build persona choices for new agents
    let persona_options: Vec<Choice> = PERSONAS
        .iter()
        .map(|(key, desc)| Choice {
            key: key.to_string(),
            label: key.replace('_', " "),
            description: desc.to_string(),
            command: Some(format!("predict-agent set-persona {}", key)),
        })
        .collect();

    // Prompt persona selection whenever not set (not just first run)
    if persona == "none" || persona.is_empty() {
        log_info!("preflight: no persona set, prompting selection");
        Output::success(
            format!(
                "Ready! But no persona set — choose one to shape your analysis style. {} open markets, {} chips.",
                open_markets, balance
            ),
            json!({
                "status": "needs_persona",
                "address": address,
                "open_markets": open_markets,
                "total_predictions": total_predictions,
                "balance": balance,
                "persona": persona,
                "wallet_id": wallet_id,
            }),
            Internal {
                next_action: "select_persona".into(),
                next_command: Some("predict-agent set-persona <PERSONA>".into()),
                progress: Some("4/4".into()),
                options: Some(persona_options),
                ..Default::default()
            },
        )
        .print();
    } else {
        Output::success(
            format!(
                "Ready. {} open markets. {} total predictions. Balance: {} chips.",
                open_markets, total_predictions, balance
            ),
            json!({
                "status": "ready",
                "address": address,
                "open_markets": open_markets,
                "total_predictions": total_predictions,
                "balance": balance,
                "persona": persona,
                "wallet_id": wallet_id,
            }),
            Internal {
                next_action: "fetch_context".into(),
                next_command: Some("predict-agent context".into()),
                progress: Some("4/4".into()),
                ..Default::default()
            },
        )
        .print();
    }

    Ok(())
}
