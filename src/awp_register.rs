/// AWP network registration — check and auto-register via gasless relay.
///
/// Flow:
///   1. JSON-RPC address.check → isRegistered?
///   2. If not: registry.get → nonce.get → build EIP-712 SetRecipient → sign → relay
///   3. Poll address.check until confirmed

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::thread;
use std::time::Duration;

use crate::auth::find_awp_wallet;
use crate::{log_debug, log_error, log_info, log_warn};

/// Safely truncate a string to a maximum number of characters (not bytes).
fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        format!("{}...(truncated)", s.chars().take(max_chars).collect::<String>())
    }
}

const AWP_API_BASE: &str = "https://api.awp.sh/v2";
const AWP_RELAY_BASE: &str = "https://api.awp.sh/api";
const CHAIN_ID: u64 = 8453; // Base mainnet
const POLL_ATTEMPTS: u32 = 5;
const POLL_INTERVAL_SECS: u64 = 2;

pub struct RegistrationResult {
    pub registered: bool,
    pub auto_registered: bool,
    pub message: String,
}

/// Check AWP registration status. Returns Ok with status, never panics on network errors.
pub fn check_registration(address: &str) -> Result<bool> {
    log_debug!("awp_register: checking registration for {} on chain {}", address, CHAIN_ID);
    let client = build_client();
    let resp = awp_jsonrpc(&client, "address.check", json!({
        "address": address,
        "chainId": CHAIN_ID,
    }))?;

    let registered = is_registered(&resp);
    log_debug!("awp_register: address.check result: registered={}, raw={}", registered, resp);
    Ok(registered)
}

/// Check and auto-register if needed. Gasless, free.
pub fn ensure_registered(address: &str, wallet_token: &str) -> Result<RegistrationResult> {
    let client = build_client();

    // Step 1: check current status
    log_info!("awp_register: step 1/7 — checking current registration status...");
    let check = awp_jsonrpc(&client, "address.check", json!({
        "address": address,
        "chainId": CHAIN_ID,
    }))?;

    if is_registered(&check) {
        log_info!("awp_register: already registered on AWP network");
        return Ok(RegistrationResult {
            registered: true,
            auto_registered: false,
            message: "Already registered on AWP network.".into(),
        });
    }
    log_info!("awp_register: not registered, starting auto-registration...");

    // Step 2: get registry for contract address
    log_info!("awp_register: step 2/7 — fetching registry contract address...");
    let registry = awp_jsonrpc(&client, "registry.get", json!({
        "chainId": CHAIN_ID,
    }))?;
    log_debug!("awp_register: registry.get response: {}", registry);

    let verifying_contract = registry
        .get("awpRegistry")
        .and_then(|v| v.as_str())
        .context("registry.get missing awpRegistry address — unexpected API response")?
        .to_string();
    log_info!("awp_register: registry contract = {}", verifying_contract);

    // Step 3: get nonce
    log_info!("awp_register: step 3/7 — fetching nonce...");
    let nonce_resp = awp_jsonrpc(&client, "nonce.get", json!({
        "address": address,
        "chainId": CHAIN_ID,
    }))?;
    log_debug!("awp_register: nonce.get response: {}", nonce_resp);

    let nonce = nonce_resp
        .get("nonce")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0);
    log_info!("awp_register: nonce = {}", nonce);

    let deadline = chrono::Utc::now().timestamp() as u64 + 3600;
    log_debug!("awp_register: deadline = {} (1 hour from now)", deadline);

    // Step 4: build EIP-712 typed data for SetRecipient
    log_info!("awp_register: step 4/7 — building EIP-712 typed data...");
    let typed_data = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "SetRecipient": [
                {"name": "user", "type": "address"},
                {"name": "recipient", "type": "address"},
                {"name": "nonce", "type": "uint256"},
                {"name": "deadline", "type": "uint256"}
            ]
        },
        "primaryType": "SetRecipient",
        "domain": {
            "name": "AWPRegistry",
            "version": "1",
            "chainId": CHAIN_ID,
            "verifyingContract": verifying_contract
        },
        "message": {
            "user": address,
            "recipient": address,
            "nonce": nonce,
            "deadline": deadline
        }
    });
    log_debug!("awp_register: EIP-712 typed data: {}", serde_json::to_string_pretty(&typed_data).unwrap_or_default());

    // Step 5: sign with awp-wallet
    log_info!("awp_register: step 5/7 — signing EIP-712 data with awp-wallet...");
    let signature = sign_typed_data(wallet_token, &typed_data)?;
    log_info!("awp_register: signature obtained ({}...{})",
        &signature[..8.min(signature.len())],
        &signature[signature.len().saturating_sub(6)..]
    );

    // Step 6: submit to gasless relay
    log_info!("awp_register: step 6/7 — submitting to gasless relay...");
    let relay_url = format!("{}/relay/set-recipient", AWP_RELAY_BASE);
    let relay_body = json!({
        "user": address,
        "recipient": address,
        "nonce": nonce,
        "deadline": deadline,
        "chainId": CHAIN_ID,
        "signature": signature,
    });
    log_debug!("awp_register: relay POST {} with body: {}", relay_url, relay_body);

    let relay_resp = client
        .post(&relay_url)
        .header("Content-Type", "application/json")
        .json(&relay_body)
        .send()
        .context(format!("Failed to call registration relay at {}", relay_url))?;

    let relay_status = relay_resp.status();
    if !relay_status.is_success() {
        let body = relay_resp.text().unwrap_or_default();
        log_error!(
            "awp_register: relay returned HTTP {} — body: {}",
            relay_status,
            body
        );
        bail!(
            "Registration relay returned HTTP {}: {}",
            relay_status,
            body
        );
    }
    let relay_body_text = relay_resp.text().unwrap_or_default();
    log_info!("awp_register: relay accepted (HTTP {})", relay_status);
    log_debug!("awp_register: relay response: {}", relay_body_text);

    // Step 7: poll until confirmed
    log_info!("awp_register: step 7/7 — polling for confirmation ({} attempts, {}s interval)...",
        POLL_ATTEMPTS, POLL_INTERVAL_SECS);
    for attempt in 0..POLL_ATTEMPTS {
        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));

        log_debug!("awp_register: poll attempt {}/{}", attempt + 1, POLL_ATTEMPTS);
        match awp_jsonrpc(&client, "address.check", json!({
            "address": address,
            "chainId": CHAIN_ID,
        })) {
            Ok(refreshed) if is_registered(&refreshed) => {
                log_info!("awp_register: registration confirmed on attempt {}", attempt + 1);
                return Ok(RegistrationResult {
                    registered: true,
                    auto_registered: true,
                    message: "Auto-registered on AWP network (gasless).".into(),
                });
            }
            Ok(refreshed) => {
                log_debug!("awp_register: poll {}: not yet confirmed: {}", attempt + 1, refreshed);
                if attempt == POLL_ATTEMPTS - 1 {
                    log_warn!("awp_register: not confirmed after {} attempts, assuming success (relay accepted)", POLL_ATTEMPTS);
                    return Ok(RegistrationResult {
                        registered: true, // optimistic
                        auto_registered: true,
                        message: "Registration submitted. Confirmation pending.".into(),
                    });
                }
            }
            Err(e) => {
                log_warn!("awp_register: poll {} failed: {}", attempt + 1, e);
                if attempt == POLL_ATTEMPTS - 1 {
                    log_warn!("awp_register: poll failed on last attempt, assuming success");
                    return Ok(RegistrationResult {
                        registered: true,
                        auto_registered: true,
                        message: "Registration submitted. Confirmation pending.".into(),
                    });
                }
            }
        }
    }

    log_error!("awp_register: registration not confirmed after all attempts");
    Ok(RegistrationResult {
        registered: false,
        auto_registered: false,
        message: "Registration submitted but not yet confirmed.".into(),
    })
}

fn is_registered(check: &Value) -> bool {
    check
        .get("isRegistered")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || check
            .get("isRegisteredUser")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

fn build_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("predict-agent/0.1.0")
        .build()
        .expect("failed to build HTTP client")
}

fn awp_jsonrpc(client: &Client, method: &str, params: Value) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    });

    log_debug!("awp_register: JSON-RPC {} -> {}", method, AWP_API_BASE);
    let start = std::time::Instant::now();

    let resp = client
        .post(AWP_API_BASE)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context(format!(
            "AWP API call {} failed: cannot reach {} — check network connectivity",
            method, AWP_API_BASE
        ))?;

    let elapsed = start.elapsed();
    let status = resp.status();
    let resp_text = resp.text().unwrap_or_default();
    log_debug!(
        "awp_register: JSON-RPC {} <- HTTP {} ({:.1}ms, {} bytes)",
        method,
        status,
        elapsed.as_millis(),
        resp_text.len()
    );

    let result: Value = serde_json::from_str(&resp_text).context(format!(
        "AWP API {} returned invalid JSON (HTTP {}): {}",
        method,
        status,
        truncate_str(&resp_text, 500)
    ))?;

    if let Some(err) = result.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        log_error!("awp_register: JSON-RPC {} error: code={}, message={}", method, code, msg);
        bail!("AWP API error ({}): {} (code: {})", method, msg, code);
    }

    result
        .get("result")
        .cloned()
        .context(format!(
            "AWP API {} returned no 'result' field. Full response: {}",
            method,
            serde_json::to_string(&result).unwrap_or_default()
        ))
}

fn sign_typed_data(wallet_token: &str, typed_data: &Value) -> Result<String> {
    let wallet_bin = find_awp_wallet()?;
    let data_str = serde_json::to_string(typed_data)?;

    log_debug!(
        "awp_register: calling {} sign-typed-data --token *** --data <{} bytes>",
        wallet_bin.display(),
        data_str.len()
    );

    let output = std::process::Command::new(&wallet_bin)
        .args([
            "sign-typed-data",
            "--token",
            wallet_token,
            "--data",
            &data_str,
        ])
        .output()
        .context(format!(
            "Failed to run awp-wallet sign-typed-data at {}. Is awp-wallet installed?",
            wallet_bin.display()
        ))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        log_error!(
            "awp_register: awp-wallet sign-typed-data failed (exit {:?})\n  stderr: {}\n  stdout: {}",
            output.status.code(),
            stderr.trim(),
            stdout.trim()
        );
        bail!(
            "awp-wallet sign-typed-data failed (exit {}): {}",
            output.status.code().map(|c| c.to_string()).unwrap_or("unknown".into()),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    log_debug!("awp_register: sign-typed-data output: {}", stdout.trim());

    let v: Value = serde_json::from_str(&stdout).context(format!(
        "awp-wallet sign-typed-data returned invalid JSON. Raw output: {}",
        stdout.trim()
    ))?;
    let sig = v["signature"].as_str().context(format!(
        "awp-wallet response missing 'signature' field. Got: {}",
        serde_json::to_string_pretty(&v).unwrap_or_default()
    ))?;
    Ok(sig.to_string())
}
