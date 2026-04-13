/// admin-chip-feed — trigger chip feed for zero-balance agents (admin only)

use anyhow::Result;
use serde_json::json;

use crate::client::ApiClient;
use crate::output::{Internal, Output};
use crate::{log_error, log_info};

pub fn run(server_url: &str) -> Result<()> {
    log_info!("admin-chip-feed: triggering chip feed on {}", server_url);
    let client = ApiClient::new(server_url.to_string())?;

    let resp = match client.post_auth_empty("/admin/v1/chip-feed") {
        Ok(v) => v,
        Err(e) => {
            log_error!("admin-chip-feed: failed: {}", e);
            Output::error_with_debug(
                format!("Admin chip-feed failed: {e}"),
                "ADMIN_FAILED",
                "auth",
                false,
                "Check ADMIN_ADDRESSES env var on server.",
                json!({
                    "error": format!("{e}"),
                }),
                Internal::default(),
            )
            .print();
            return Ok(());
        }
    };

    let data = resp.get("data").cloned().unwrap_or(json!({}));
    let fed = data.get("fed").and_then(|v| v.as_i64()).unwrap_or(0);
    let agents = data.get("agents").cloned().unwrap_or(json!([]));

    log_info!("admin-chip-feed: fed {} agents", fed);

    Output::success(
        format!("Chip feed complete. {} agents topped up.", fed),
        data,
        Internal::default(),
    )
    .print();

    Ok(())
}
