/// EIP-191 personal_sign authentication for predict-agent.
///
/// Signing modes (in priority order):
///   1. AWP_PRIVATE_KEY=0x{hex}   — direct ECDSA signing (dev/test)
///   2. AWP_DEV_MODE=true         — dev mode, no real signing (matches server dev bypass)
///   3. awp-wallet subprocess     — production (calls `awp-wallet sign-message ...`)
///
/// Diagnostic output goes to stderr via log_debug!/log_info!/log_error!.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use sha3::{Digest, Keccak256};
use std::path::PathBuf;

use crate::{log_debug, log_error, log_info};

pub struct AuthHeaders {
    pub address: String,
    pub timestamp: String,
    pub signature: String,
}

/// Build auth headers for a request. Timestamp is freshly generated.
/// The signature now includes method, path, and body_hash to prevent replay attacks.
pub fn build_auth_headers(
    address: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<AuthHeaders> {
    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Compute SHA-256 hash of body (matches server's body_hash computation)
    use sha2::{Digest as Sha2Digest, Sha256};
    let body_hash = hex::encode(Sha256::digest(body));

    log_debug!(
        "Building auth headers: address={}, method={}, path={}, body_hash={}, timestamp={}",
        address,
        method,
        path,
        &body_hash[..16],
        timestamp
    );
    let signature = sign_message(address, &timestamp, method, path, &body_hash)?;
    log_debug!("Auth signature generated: {}...{}", &signature[..8.min(signature.len())], &signature[signature.len().saturating_sub(6)..]);
    Ok(AuthHeaders {
        address: address.to_string(),
        timestamp,
        signature,
    })
}

fn sign_message(
    address: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body_hash: &str,
) -> Result<String> {
    let addr_lower = address.to_lowercase();

    // Mode 1: direct private key
    if std::env::var("AWP_PRIVATE_KEY").is_ok() {
        log_debug!("Signing mode: AWP_PRIVATE_KEY (direct ECDSA)");
        let pk_hex = std::env::var("AWP_PRIVATE_KEY").unwrap();
        return sign_with_key(&pk_hex, &addr_lower, timestamp, method, path, body_hash);
    }

    // Mode 2: dev mode bypass
    if std::env::var("AWP_DEV_MODE").as_deref() == Ok("true")
        || std::env::var("AWP_DEV_MODE").as_deref() == Ok("1")
    {
        log_debug!("Signing mode: AWP_DEV_MODE (dev bypass, signature='dev')");
        return Ok("dev".to_string());
    }

    // Mode 3: awp-wallet subprocess
    log_debug!("Signing mode: awp-wallet subprocess");
    sign_with_wallet(&addr_lower, timestamp, method, path, body_hash)
}

fn sign_with_key(
    pk_hex: &str,
    addr_lower: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body_hash: &str,
) -> Result<String> {
    let pk_hex = pk_hex.strip_prefix("0x").unwrap_or(pk_hex);
    let pk_bytes = hex::decode(pk_hex).context("Invalid AWP_PRIVATE_KEY hex — must be a valid hex string (with or without 0x prefix)")?;
    let signing_key = SigningKey::from_slice(&pk_bytes)
        .context("Invalid private key bytes — expected 32-byte secp256k1 private key")?;

    let message = format!(
        "AWP Predict WorkNet\nAddress: {}\nTimestamp: {}\nMethod: {}\nPath: {}\nBody-Hash: {}",
        addr_lower,
        timestamp,
        method.to_uppercase(),
        path,
        body_hash
    );
    log_debug!("EIP-191 message to sign:\n{}", message);

    // EIP-191 personal_sign hash
    let msg_hash = personal_sign_hash(message.as_bytes());

    // Sign the prehash
    let (sig, recovery_id): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) = signing_key
        .sign_prehash(&msg_hash)
        .context("ECDSA signing failed — this should not happen with a valid key")?;

    // Build 65-byte signature: r || s || v (v = recovery_id + 27)
    let mut sig_bytes = [0u8; 65];
    sig_bytes[..64].copy_from_slice(&sig.to_bytes());
    sig_bytes[64] = recovery_id.to_byte() + 27;

    Ok(format!("0x{}", hex::encode(sig_bytes)))
}

fn sign_with_wallet(
    addr_lower: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body_hash: &str,
) -> Result<String> {
    let token = std::env::var("AWP_WALLET_TOKEN").context(
        "AWP_WALLET_TOKEN not set. You need to unlock your wallet first.\n\
         Run: awp-wallet unlock --duration 86400 --scope full\n\
         Then: export AWP_WALLET_TOKEN=<token from output>",
    )?;
    log_debug!("AWP_WALLET_TOKEN present (length={})", token.len());

    let message = format!(
        "AWP Predict WorkNet\nAddress: {}\nTimestamp: {}\nMethod: {}\nPath: {}\nBody-Hash: {}",
        addr_lower,
        timestamp,
        method.to_uppercase(),
        path,
        body_hash
    );
    log_debug!("EIP-191 message to sign:\n{}", message);

    let wallet_bin = find_awp_wallet()?;
    log_debug!("Calling: {} sign-message --token ***  --message <...>", wallet_bin.display());

    let output = std::process::Command::new(&wallet_bin)
        .args(["sign-message", "--token", &token, "--message", &message])
        .output()
        .context(format!(
            "Failed to execute awp-wallet at {}. Is it installed and executable?",
            wallet_bin.display()
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        log_error!(
            "awp-wallet sign-message failed (exit code: {:?})\n  stderr: {}\n  stdout: {}",
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        );
        // Check for common failure patterns
        if stderr.contains("expired") || stderr.contains("invalid token") {
            bail!(
                "awp-wallet session expired or token invalid. Re-unlock your wallet:\n\
                 awp-wallet unlock --duration 86400 --scope full\n\
                 Original error: {}",
                stderr.trim()
            );
        }
        bail!(
            "awp-wallet sign-message failed (exit {}): {}",
            output.status.code().map(|c| c.to_string()).unwrap_or("unknown".into()),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    log_debug!("awp-wallet sign-message output: {}", stdout.trim());

    let v: serde_json::Value = serde_json::from_str(&stdout).context(format!(
        "awp-wallet returned invalid JSON. Raw output: {}",
        stdout.trim()
    ))?;

    // Check that signing key matches expected address
    if let Some(signer) = v["signer"].as_str() {
        let signer_lower = signer.to_lowercase();
        if signer_lower != addr_lower {
            log_error!(
                "WALLET MISMATCH: receive returned {} but sign-message used {}",
                addr_lower,
                signer_lower
            );
            bail!(
                "Wallet address mismatch detected!\n\
                 receive returned:    {}\n\
                 sign-message used:   {}\n\n\
                 This usually means AWP_AGENT_ID or AWP_SESSION_ID changed between calls,\n\
                 causing different wallets to be used. Make sure these env vars are stable.\n\
                 Try: unset AWP_AGENT_ID AWP_SESSION_ID && predict-agent preflight",
                addr_lower,
                signer_lower
            );
        }
        log_debug!("Signer address verified: {}", signer_lower);
    }

    let sig = v["signature"]
        .as_str()
        .context(format!(
            "awp-wallet response missing 'signature' field. Got: {}",
            serde_json::to_string_pretty(&v).unwrap_or_default()
        ))?;
    Ok(sig.to_string())
}

/// Get wallet address from awp-wallet or AWP_ADDRESS env var.
pub fn get_address() -> Result<String> {
    // Direct env var (dev/test)
    if let Ok(addr) = std::env::var("AWP_ADDRESS") {
        log_debug!("Address source: AWP_ADDRESS env var = {}", addr);
        return Ok(addr.to_lowercase());
    }

    // Derive from private key
    if let Ok(pk_hex) = std::env::var("AWP_PRIVATE_KEY") {
        log_debug!("Address source: derived from AWP_PRIVATE_KEY");
        return derive_address_from_key(&pk_hex);
    }

    // awp-wallet subprocess
    log_debug!("Address source: awp-wallet receive");
    get_address_from_wallet()
}

fn derive_address_from_key(pk_hex: &str) -> Result<String> {
    let pk_hex = pk_hex.strip_prefix("0x").unwrap_or(pk_hex);
    let pk_bytes = hex::decode(pk_hex).context("Invalid AWP_PRIVATE_KEY hex")?;
    let signing_key = SigningKey::from_slice(&pk_bytes).context("Invalid private key bytes")?;
    let verifying_key = signing_key.verifying_key();
    let point = verifying_key.to_encoded_point(false);
    let pubkey_bytes = &point.as_bytes()[1..]; // skip 0x04 prefix
    let hash = Keccak256::digest(pubkey_bytes);
    let addr = format!("0x{}", hex::encode(&hash[12..]));
    log_debug!("Derived address from private key: {}", addr);
    Ok(addr)
}

fn get_address_from_wallet() -> Result<String> {
    let agent_id = std::env::var("AWP_AGENT_ID").unwrap_or_default();
    let token = std::env::var("AWP_WALLET_TOKEN").unwrap_or_default();

    let mut args = vec!["receive"];
    if !agent_id.is_empty() {
        args.extend_from_slice(&["--agent", &agent_id]);
        log_debug!("Using AWP_AGENT_ID: {}", agent_id);
    }
    // Note: `awp-wallet receive` does NOT accept --token.
    // It reads the address directly from keystore metadata.
    log_debug!("AWP_WALLET_TOKEN present: {}", !token.is_empty());

    let wallet_bin = find_awp_wallet()?;
    log_debug!("Calling: {} {}", wallet_bin.display(), args.join(" "));

    let output = std::process::Command::new(&wallet_bin)
        .args(&args)
        .output()
        .context(format!(
            "Failed to execute awp-wallet at {}. Is it installed?",
            wallet_bin.display()
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        log_error!(
            "awp-wallet receive failed (exit code: {:?})\n  stderr: {}\n  stdout: {}",
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        );
        bail!(
            "awp-wallet is locked or not set up. Run:\n\
             awp-wallet unlock --duration 86400 --scope full\n\
             Error: {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    log_debug!("awp-wallet receive output: {}", stdout.trim());

    let v: serde_json::Value = serde_json::from_str(&stdout).context(format!(
        "awp-wallet returned invalid JSON from 'receive'. Raw output: {}",
        stdout.trim()
    ))?;
    let addr = v["eoaAddress"]
        .as_str()
        .or_else(|| v["address"].as_str())
        .context(format!(
            "awp-wallet response missing 'eoaAddress' field. Got keys: {:?}",
            v.as_object().map(|o| o.keys().collect::<Vec<_>>())
        ))?;
    log_debug!("Resolved wallet address: {}", addr);
    Ok(addr.to_lowercase())
}

fn personal_sign_hash(message: &[u8]) -> Vec<u8> {
    let prefix = format!("\x19Ethereum Signed Message:\n{}", message.len());
    let mut hasher = Keccak256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(message);
    hasher.finalize().to_vec()
}

/// Find awp-wallet binary. Checks PATH first, then well-known install locations.
pub fn find_awp_wallet() -> Result<PathBuf> {
    // Check PATH
    if let Ok(path) = which("awp-wallet") {
        log_debug!("Found awp-wallet in PATH: {}", path.display());
        return Ok(path);
    }

    // Search well-known locations (learned from awp-skill)
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/awp-wallet"),
        format!("{home}/.npm-global/bin/awp-wallet"),
        format!("{home}/.yarn/bin/awp-wallet"),
        "/usr/local/bin/awp-wallet".to_string(),
        "/usr/bin/awp-wallet".to_string(),
    ];

    log_debug!("awp-wallet not in PATH, searching well-known locations...");
    for path_str in &candidates {
        let path = PathBuf::from(path_str);
        log_debug!("  Checking: {} — {}", path_str, if path.is_file() { "FOUND" } else { "not found" });
        if path.is_file() {
            log_info!("Found awp-wallet at {}", path.display());
            return Ok(path);
        }
    }

    log_error!(
        "awp-wallet not found. Searched PATH and: {:?}",
        candidates
    );
    bail!(
        "awp-wallet not found in PATH or standard locations.\n\
         Searched: {}\n\
         Install it: curl -sSL https://install.awp.sh/wallet | bash",
        candidates.join(", ")
    )
}

/// Minimal which(1) implementation.
fn which(binary: &str) -> Result<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = PathBuf::from(dir).join(binary);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{binary} not found in PATH")
}

/// Attempt to refresh the wallet token by calling `awp-wallet unlock`.
/// Returns the new token on success, or an error if refresh fails.
pub fn refresh_wallet_token() -> Result<String> {
    log_info!("Attempting to refresh wallet token...");

    let wallet_bin = find_awp_wallet()?;

    // Build args for unlock
    let mut args = vec![
        "unlock",
        "--duration", "86400",
        "--scope", "full",
        "--raw",
    ];

    // Pass agent ID if set
    let agent_id = std::env::var("AWP_AGENT_ID").unwrap_or_default();
    if !agent_id.is_empty() {
        args.push("--agent");
        args.push(&agent_id);
    }

    log_debug!("Calling: {} {}", wallet_bin.display(), args.join(" "));

    let output = std::process::Command::new(&wallet_bin)
        .args(&args)
        .output()
        .context("Failed to execute awp-wallet unlock")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log_error!("awp-wallet unlock failed: {}", stderr.trim());
        bail!("Failed to refresh wallet token: {}", stderr.trim());
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if token.is_empty() {
        bail!("awp-wallet unlock returned empty token");
    }

    // Update environment variable
    std::env::set_var("AWP_WALLET_TOKEN", &token);
    log_info!("Wallet token refreshed successfully (length={})", token.len());

    Ok(token)
}
