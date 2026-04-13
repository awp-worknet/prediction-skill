/// set-persona — update the agent's persona (7-day cooldown).

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_debug, log_error, log_info};

// Predefined personas with known sizing behavior
pub const PREDEFINED_PERSONAS: &[&str] = &[
    "degen",
    "conservative",
    "sniper",
    "contrarian",
    "chartist",
    "macro",
    "sentiment",
];

pub fn run(server_url: &str, persona: &str) -> Result<()> {
    log_info!("set-persona: setting persona to '{}' at {}", persona, server_url);

    // Custom personas are allowed - server validates length (1-50 chars)
    if !PREDEFINED_PERSONAS.contains(&persona) {
        log_info!("set-persona: using custom persona '{}'", persona);
    }

    let client = ApiClient::new(server_url.to_string())?;

    let body = json!({ "persona": persona });
    log_debug!("set-persona: POST /api/v1/agents/me/persona with {:?}", body);

    let resp = match client.post_auth("/api/v1/agents/me/persona", &body) {
        Ok(v) => v,
        Err(e) => {
            let err_str = e.to_string();
            log_error!("set-persona: failed: {}", err_str);
            let (retryable, suggestion, code) =
                if err_str.contains("PERSONA_COOLDOWN") || err_str.contains("cooldown") {
                    (
                        false,
                        "Persona can only be changed once every 7 days.".to_string(),
                        "PERSONA_COOLDOWN",
                    )
                } else {
                    (
                        true,
                        "Check coordinator connectivity and retry.".to_string(),
                        "SET_PERSONA_FAILED",
                    )
                };
            Output::error_with_debug(
                format!("Failed to set persona: {}", extract_message(&err_str)),
                code,
                if retryable { "network" } else { "validation" },
                retryable,
                suggestion,
                json!({
                    "persona": persona,
                    "raw_error": err_str,
                    "server_url": server_url,
                }),
                Internal {
                    next_action: "fetch_context".into(),
                    next_command: Some("predict-agent context".into()),
                    ..Default::default()
                },
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));
    log_info!("set-persona: successfully set to '{}'", persona);

    Output::success(
        format!("Persona updated to '{}'. 7-day cooldown started.", persona),
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

fn extract_message(err: &str) -> String {
    if let Some(json_start) = err.find('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&err[json_start..]) {
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
