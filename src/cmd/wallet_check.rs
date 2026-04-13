/// wallet — show wallet status and safety information.
///
/// Humans can run this to understand their wallet state before taking action.
/// Outputs clear guidance on what's safe to do.

use anyhow::Result;
use serde_json::json;

use crate::output::{Internal, Output};
use crate::wallet::WalletStatus;
use crate::log_info;

pub fn run() -> Result<()> {
    log_info!("wallet: checking status...");

    let status = WalletStatus::check();

    log_info!(
        "wallet: cli={}, dir={}, keystore={}, receive={}, safe_to_init={}",
        status.cli_installed,
        status.wallet_dir_exists,
        status.has_keystore,
        status.can_receive,
        status.safe_to_init()
    );

    let (ok, user_message) = if status.can_receive {
        (true, format!(
            "Wallet ready: {}",
            status.address.as_deref().unwrap_or("unknown")
        ))
    } else if status.wallet_dir_exists || status.has_keystore {
        (false, "Wallet exists but locked. Run unlock command (see suggestion). DO NOT run init.".into())
    } else if status.cli_installed {
        (false, "No wallet found. Safe to run awp-wallet init.".into())
    } else {
        (false, "awp-wallet CLI not installed.".into())
    };

    if ok {
        Output::success(
            user_message,
            json!({
                "status": "ready",
                "address": status.address,
                "cli_path": status.cli_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "safe_to_init": status.safe_to_init(),
            }),
            Internal {
                next_action: "ready".into(),
                next_command: Some("predict-agent preflight".into()),
                progress: Some("1/4".into()),
                ..Default::default()
            },
        )
        .print();
    } else {
        Output::error_with_debug(
            user_message,
            if status.cli_installed { "WALLET_LOCKED" } else { "CLI_NOT_INSTALLED" },
            "wallet",
            false,
            status.suggestion(),
            json!({
                "cli_installed": status.cli_installed,
                "cli_path": status.cli_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "wallet_dir_exists": status.wallet_dir_exists,
                "wallet_dir": WalletStatus::wallet_dir().to_string_lossy(),
                "has_keystore": status.has_keystore,
                "can_receive": status.can_receive,
                "safe_to_init": status.safe_to_init(),
                "human_status": status.human_status,
            }),
            Internal {
                next_action: "configure_wallet".into(),
                next_command: Some(status.setup_command().into()),
                progress: Some("0/4".into()),
                ..Default::default()
            },
        )
        .print();
    }

    Ok(())
}
