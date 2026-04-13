/// Wallet safety checks — prevent accidental wallet overwrites.
///
/// CRITICAL: `awp-wallet init` on an existing wallet creates a NEW wallet
/// and loses access to the old one. These functions ensure we never
/// suggest `init` when a wallet already exists.

use std::path::PathBuf;

use crate::auth::find_awp_wallet;
use crate::log_debug;

/// Wallet state for safety decisions
#[derive(Debug, Clone)]
pub struct WalletStatus {
    /// awp-wallet CLI is installed
    pub cli_installed: bool,
    /// Path to awp-wallet binary (if found)
    pub cli_path: Option<PathBuf>,
    /// ~/.awp-wallet directory exists (wallet was initialized at some point)
    pub wallet_dir_exists: bool,
    /// Wallet directory contains keystore files
    pub has_keystore: bool,
    /// awp-wallet receive succeeds (wallet is unlocked/accessible)
    pub can_receive: bool,
    /// The wallet address (if accessible)
    pub address: Option<String>,
    /// Descriptive status for humans
    pub human_status: String,
}

impl WalletStatus {
    /// Check current wallet state. Never fails — always returns a status.
    pub fn check() -> Self {
        let mut status = Self {
            cli_installed: false,
            cli_path: None,
            wallet_dir_exists: false,
            has_keystore: false,
            can_receive: false,
            address: None,
            human_status: String::new(),
        };

        // Check CLI installation
        match find_awp_wallet() {
            Ok(path) => {
                status.cli_installed = true;
                status.cli_path = Some(path);
                log_debug!("wallet: CLI found at {:?}", status.cli_path);
            }
            Err(e) => {
                log_debug!("wallet: CLI not found: {}", e);
                status.human_status = "awp-wallet CLI not installed".into();
                return status;
            }
        }

        // Check wallet directory
        let wallet_dir = Self::wallet_dir();
        status.wallet_dir_exists = wallet_dir.exists();
        log_debug!("wallet: dir {:?} exists={}", wallet_dir, status.wallet_dir_exists);

        if status.wallet_dir_exists {
            // Check for keystore files
            if let Ok(entries) = std::fs::read_dir(&wallet_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    // Keystore files are typically JSON or have specific patterns
                    if name_str.ends_with(".json")
                        || name_str.starts_with("keystore")
                        || name_str.starts_with("UTC--")
                        || entry.path().is_dir()
                    {
                        status.has_keystore = true;
                        log_debug!("wallet: found keystore indicator: {}", name_str);
                        break;
                    }
                }
            }
        }

        // Try to get address via receive
        if let Some(ref cli_path) = status.cli_path {
            if let Ok(output) = std::process::Command::new(cli_path)
                .args(["receive"])
                .output()
            {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&stdout) {
                        if let Some(addr) = v["eoaAddress"].as_str().or(v["address"].as_str()) {
                            status.can_receive = true;
                            status.address = Some(addr.to_lowercase());
                            log_debug!("wallet: receive succeeded, address={}", addr);
                        }
                    }
                } else {
                    log_debug!(
                        "wallet: receive failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }

        // Determine human-readable status
        status.human_status = match (
            status.wallet_dir_exists,
            status.has_keystore,
            status.can_receive,
        ) {
            (false, _, _) => "No wallet — safe to run awp-wallet init".into(),
            (true, false, false) => {
                "Wallet directory exists but empty — may need init or recovery".into()
            }
            (true, true, false) => {
                "Wallet exists but not accessible — check awp-wallet installation (DO NOT run init)".into()
            }
            (true, _, true) => format!(
                "Wallet ready: {}",
                status.address.as_deref().unwrap_or("unknown")
            ),
        };

        status
    }

    /// Returns the wallet directory path (~/.awp-wallet)
    pub fn wallet_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        PathBuf::from(home).join(".awp-wallet")
    }

    /// Is it safe to run `awp-wallet init`?
    /// Returns false if any wallet data exists (to prevent accidental overwrite)
    pub fn safe_to_init(&self) -> bool {
        !self.wallet_dir_exists && !self.has_keystore
    }

    /// Get the appropriate next command for wallet setup
    pub fn setup_command(&self) -> &'static str {
        if self.can_receive {
            // Already working
            "predict-agent preflight"
        } else if self.wallet_dir_exists || self.has_keystore {
            // Wallet exists but not accessible — check installation
            "predict-agent preflight"
        } else if self.cli_installed {
            // No wallet — safe to init (no unlock needed anymore)
            "awp-wallet init"
        } else {
            // Need to install CLI first
            "curl -sSL https://raw.githubusercontent.com/predictAworknet/prediction-skill/main/install.sh | sh"
        }
    }

    /// Get suggestion text for error messages
    pub fn suggestion(&self) -> String {
        if self.can_receive {
            "Wallet is ready. Run: predict-agent preflight".into()
        } else if self.wallet_dir_exists || self.has_keystore {
            "Wallet exists but not accessible. Check awp-wallet installation.\n\
             IMPORTANT: Do NOT run awp-wallet init — that would overwrite your existing wallet!".into()
        } else if self.cli_installed {
            "No wallet found. Run: awp-wallet init".into()
        } else {
            "awp-wallet not installed. Install it first, then run init.".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wallet_dir() {
        let dir = WalletStatus::wallet_dir();
        assert!(dir.to_string_lossy().contains(".awp-wallet"));
    }
}
