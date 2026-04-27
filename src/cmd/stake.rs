/// stake — show current on-chain stake status + 3-path guidance.
///
/// Predict requires ≥1000 AWP allocated to the Predict WorkNet (or to the
/// KYA WorkNet, when sponsored) before submissions are accepted. This
/// command reads the live state via the server's read API and prints a
/// human-friendly summary plus next-step instructions.

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_error, log_info};

pub fn run(server_url: &str) -> Result<()> {
    log_info!("stake: fetching stake status from {}", server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let resp = match client.get_auth("/api/v1/agents/me/stake") {
        Ok(v) => v,
        Err(e) => {
            log_error!("stake: failed to fetch: {}", e);
            Output::error_with_debug(
                format!("Failed to fetch stake status: {e}"),
                "STAKE_FETCH_FAILED",
                "network",
                true,
                "Check coordinator connectivity.",
                json!({
                    "server_url": server_url,
                    "error_detail": format!("{e}"),
                }),
                Internal {
                    next_action: "retry".into(),
                    next_command: Some("predict-agent stake".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));

    let agent = data.get("agent_address").and_then(|v| v.as_str()).unwrap_or("unknown");
    let current_awp = data.get("current_stake_awp").and_then(|v| v.as_str()).unwrap_or("0");
    let required_awp = data.get("required_stake_awp").and_then(|v| v.as_str()).unwrap_or("1000");
    let eligible = data.get("eligible").and_then(|v| v.as_bool()).unwrap_or(false);
    let lock_until = data.get("lock_min_until").and_then(|v| v.as_str()).map(|s| s.to_string());
    let backers = data.get("backers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let gate_mode = data.get("gate_mode").and_then(|v| v.as_str()).unwrap_or("off");

    // Render the human-facing summary as a multiline message.
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Agent:           {agent}"));
    lines.push(format!("Current stake:   {current_awp} AWP"));
    lines.push(format!("Required:        {required_awp} AWP"));
    lines.push(format!(
        "Status:          {}",
        if eligible { "✓ ELIGIBLE" } else { "✗ NOT ELIGIBLE" }
    ));
    lines.push(format!("Gate mode:       {gate_mode}"));
    if let Some(t) = &lock_until {
        lines.push(format!("Lock expires at: {t}"));
    }
    if !backers.is_empty() {
        lines.push(format!("Backers ({}):", backers.len()));
        for b in &backers {
            lines.push(format!(
                "  • staker {}  →  {} AWP  (worknet {})",
                b.get("staker").and_then(|v| v.as_str()).unwrap_or("?"),
                b.get("amount_awp").and_then(|v| v.as_str()).unwrap_or("?"),
                b.get("worknet_id").and_then(|v| v.as_str()).unwrap_or("?"),
            ));
        }
    }

    if eligible {
        lines.push(String::new());
        lines.push("You're staked. Run `predict-agent context` to start.".into());

        Output::success(
            lines.join("\n"),
            data.clone(),
            Internal {
                next_action: "ready".into(),
                next_command: Some("predict-agent context".into()),
                ..Default::default()
            },
        )
        .print();
        return Ok(());
    }

    // Not eligible — append the three-path guidance.
    lines.push(String::new());
    lines.push("To become eligible — pick whichever path fits you:".into());
    lines.push(String::new());
    lines.push("──── [A] Easiest — official AWP web UI ────".into());
    lines.push("    https://awp.pro/staking".into());
    lines.push(format!(
        "    Connect your wallet, lock ≥{required_awp} AWP, and allocate to"
    ));
    lines.push(format!("    (your agent {agent}, worknetId 845300000003)."));
    lines.push("    The UI walks you through every step.".into());
    lines.push(String::new());
    lines.push("──── [B] No-AWP path — KYA delegated staking ────".into());
    lines.push("    https://kya.link/".into());
    lines.push("    Complete KYA's verification (KYC / Twitter); KYA sponsors".into());
    lines.push(format!(
        "    stake to (your agent {agent}, worknetId 845300000012) on your"
    ));
    lines.push("    behalf — you don't need to hold AWP yourself.".into());
    lines.push(String::new());
    lines.push("──── [C] Programmatic — direct contract calls (advanced) ────".into());
    lines.push("    Base mainnet (chainId 8453):".into());
    lines.push(
        "    1) veAWP.deposit(amount, lockDuration)  \
         contract: 0x0000b534C63D78212f1BDCc315165852793A00A8"
            .into(),
    );
    lines.push(
        "    2) AWPAllocator.allocate(staker=you, agent=this_agent_address, \
         worknetId=845300000003, amount=1000e18)"
            .into(),
    );
    lines.push("       contract: 0x0000D6BB5e040E35081b3AaF59DD71b21C9800AA".into());
    lines.push(String::new());
    lines.push("After staking, wait ~10s, then re-run: predict-agent stake".into());

    Output::error_with_debug(
        lines.join("\n"),
        "NOT_STAKED",
        "stake",
        false,
        "Complete one of the three paths above, then re-run `predict-agent stake`.",
        data.clone(),
        Internal {
            next_action: "stake_required".into(),
            next_command: Some("predict-agent stake".into()),
            ..Default::default()
        },
    )
    .print();

    Ok(())
}
