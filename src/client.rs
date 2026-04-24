/// HTTP client wrapper for the Predict WorkNet Coordinator API.
///
/// All requests are logged to stderr:
///   - Request: method, URL, auth headers present
///   - Response: status code, body size, elapsed time
///   - On error: full response body for diagnosis
use anyhow::{bail, Context, Result};
use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;
use std::time::Instant;

use crate::auth::{build_auth_headers, get_address};
use crate::{log_debug, log_error, log_warn};

/// Safely truncate a string to a maximum number of characters (not bytes).
/// This avoids panics when slicing multi-byte UTF-8 characters like → or emoji.
fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        format!("{}...(truncated)", s.chars().take(max_chars).collect::<String>())
    }
}

pub struct ApiClient {
    pub base_url: String,
    pub address: String,
    client: Client,
}

impl ApiClient {
    pub fn new(base_url: String) -> Result<Self> {
        log_debug!("Creating API client for {}", base_url);
        let address = get_address().context(
            "Could not determine wallet address. Set AWP_ADDRESS, AWP_PRIVATE_KEY, or configure awp-wallet.",
        )?;
        log_debug!("Resolved wallet address: {}", address);
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("predict-agent/0.1.0")
            .build()?;
        Ok(Self {
            base_url,
            address,
            client,
        })
    }

    /// GET an unauthenticated endpoint.
    pub fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        log_debug!("GET {} (no auth)", url);
        let start = Instant::now();

        let resp = self.client.get(&url).send().context(format!(
            "GET {} failed: network error (is the coordinator running at {}?)",
            path, self.base_url
        ))?;

        let elapsed = start.elapsed();
        log_debug!("GET {} -> {} ({:.1}ms)", path, resp.status(), elapsed.as_millis());
        self.parse_response(resp, "GET", &url, elapsed)
    }

    /// GET an authenticated endpoint.
    pub fn get_auth(&self, path: &str) -> Result<Value> {
        // Strip query params for signing (server verifies path only, not query string)
        let sign_path = path.split('?').next().unwrap_or(path);
        // GET requests have empty body
        let auth = build_auth_headers(&self.address, "GET", sign_path, &[])?;
        let url = format!("{}{}", self.base_url, path);
        log_debug!(
            "GET {} (auth: address={}, timestamp={})",
            url,
            auth.address,
            auth.timestamp
        );
        let start = Instant::now();

        let resp = self
            .client
            .get(&url)
            .header("X-AWP-Address", &auth.address)
            .header("X-AWP-Timestamp", &auth.timestamp)
            .header("X-AWP-Signature", &auth.signature)
            .send()
            .context(format!(
                "GET {} failed: network error (is the coordinator running at {}?)",
                path, self.base_url
            ))?;

        let elapsed = start.elapsed();
        log_debug!("GET {} -> {} ({:.1}ms)", path, resp.status(), elapsed.as_millis());
        self.parse_response(resp, "GET", &url, elapsed)
    }

    /// DELETE an authenticated endpoint.
    pub fn delete_auth(&self, path: &str) -> Result<Value> {
        let auth = build_auth_headers(&self.address, "DELETE", path, &[])?;
        let url = format!("{}{}", self.base_url, path);
        log_debug!(
            "DELETE {} (auth: address={}, timestamp={})",
            url,
            auth.address,
            auth.timestamp
        );
        let start = Instant::now();

        let resp = self
            .client
            .delete(&url)
            .header("X-AWP-Address", &auth.address)
            .header("X-AWP-Timestamp", &auth.timestamp)
            .header("X-AWP-Signature", &auth.signature)
            .send()
            .context(format!(
                "DELETE {} failed: network error (is the coordinator running at {}?)",
                path, self.base_url
            ))?;

        let elapsed = start.elapsed();
        log_debug!("DELETE {} -> {} ({:.1}ms)", path, resp.status(), elapsed.as_millis());
        self.parse_response(resp, "DELETE", &url, elapsed)
    }

    /// POST an authenticated endpoint with a JSON body.
    /// Signs using the JSON bytes directly (for simple endpoints).
    pub fn post_auth(&self, path: &str, body: &Value) -> Result<Value> {
        // Serialize body to compute hash before signing
        let body_bytes = serde_json::to_vec(body).context("Failed to serialize request body")?;
        self.post_auth_with_canonical(&body_bytes, path, body)
    }

    /// POST an authenticated endpoint with a canonical body for signing.
    /// `canonical_body` is used for the signature hash, `json_body` is sent to the server.
    /// Use this when the server expects a specific canonical format for signature verification.
    pub fn post_auth_with_canonical(
        &self,
        canonical_body: &[u8],
        path: &str,
        json_body: &Value,
    ) -> Result<Value> {
        let auth = build_auth_headers(&self.address, "POST", path, canonical_body)?;
        let url = format!("{}{}", self.base_url, path);
        log_debug!(
            "POST {} (auth: address={}, timestamp={}, canonical_len={}, body_keys={:?})",
            url,
            auth.address,
            auth.timestamp,
            canonical_body.len(),
            json_body.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        let start = Instant::now();

        let json_bytes = serde_json::to_vec(json_body).context("Failed to serialize request body")?;
        let resp = self
            .client
            .post(&url)
            .header("X-AWP-Address", &auth.address)
            .header("X-AWP-Timestamp", &auth.timestamp)
            .header("X-AWP-Signature", &auth.signature)
            .header("Content-Type", "application/json")
            .body(json_bytes)
            .send()
            .context(format!(
                "POST {} failed: network error (is the coordinator running at {}?)",
                path, self.base_url
            ))?;

        let elapsed = start.elapsed();
        log_debug!("POST {} -> {} ({:.1}ms)", path, resp.status(), elapsed.as_millis());
        self.parse_response(resp, "POST", &url, elapsed)
    }

    /// POST an authenticated endpoint with no body (for admin endpoints).
    pub fn post_auth_empty(&self, path: &str) -> Result<Value> {
        let auth = build_auth_headers(&self.address, "POST", path, &[])?;
        let url = format!("{}{}", self.base_url, path);
        log_debug!(
            "POST {} (auth: address={}, timestamp={}, empty body)",
            url,
            auth.address,
            auth.timestamp,
        );
        let start = Instant::now();

        let resp = self
            .client
            .post(&url)
            .header("X-AWP-Address", &auth.address)
            .header("X-AWP-Timestamp", &auth.timestamp)
            .header("X-AWP-Signature", &auth.signature)
            .send()
            .context(format!(
                "POST {} failed: network error (is the coordinator running at {}?)",
                path, self.base_url
            ))?;

        let elapsed = start.elapsed();
        log_debug!("POST {} -> {} ({:.1}ms)", path, resp.status(), elapsed.as_millis());
        self.parse_response(resp, "POST", &url, elapsed)
    }

    fn parse_response(
        &self,
        resp: reqwest::blocking::Response,
        method: &str,
        url: &str,
        elapsed: std::time::Duration,
    ) -> Result<Value> {
        let status = resp.status();
        let body_text = resp.text().unwrap_or_default();

        log_debug!(
            "{} {} response body ({} bytes): {}",
            method,
            url,
            body_text.len(),
truncate_str(&body_text, 2000)
        );

        let body: Value = serde_json::from_str(&body_text).context(format!(
            "{} {} returned non-JSON response (status {}): {}",
            method,
            url,
            status,
truncate_str(&body_text, 500)
        ))?;

        if status == StatusCode::OK || status == StatusCode::CREATED {
            Ok(body)
        } else {
            // Log the full error response for debugging
            log_error!(
                "{} {} returned HTTP {} ({:.1}ms): {}",
                method,
                url,
                status,
                elapsed.as_millis(),
                serde_json::to_string(&body).unwrap_or_default()
            );

            // Extract the most useful error message
            let server_msg = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| body.get("message").and_then(|m| m.as_str()));

            if let Some(msg) = server_msg {
                log_warn!("Server error message: {}", msg);
            }

            bail!(
                "HTTP {}: {}",
                status,
                serde_json::to_string(&body).unwrap_or_default()
            )
        }
    }
}

/// Try to reach the server health endpoint. Returns Err if unreachable.
pub fn check_server(base_url: &str) -> Result<()> {
    let url = format!("{}/api/v1/feed/stats", base_url);
    log_debug!("Health check: GET {}", url);
    let start = Instant::now();

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("predict-agent/0.1.0")
        .build()?;
    let resp = client.get(&url).send().context(format!(
        "Cannot reach coordinator at {} — connection refused or DNS failure. \
         Check PREDICT_SERVER_URL and network connectivity.",
        base_url
    ))?;

    let elapsed = start.elapsed();
    let status = resp.status();
    log_debug!("Health check -> {} ({:.1}ms)", status, elapsed.as_millis());

    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        log_error!("Health check failed: HTTP {} — {}", status, body);
        bail!(
            "Coordinator returned HTTP {} ({}). Response: {}",
            status,
            url,
truncate_str(&body, 500)
        )
    }
}
