/// Unified JSON output structs for all predict-agent commands.
///
/// Every command outputs a single JSON object with:
///   - ok: bool
///   - user_message: human-readable summary
///   - data: command-specific payload (null on error)
///   - error: error details (null on success)
///   - _internal: LLM-facing action hints
///
/// Diagnostic logging:
///   - All commands emit structured stderr logs via `log_debug!` / `log_info!` / `log_warn!` / `log_error!`
///   - Set PREDICT_DEBUG=1 to see debug-level output (verbose)
///   - Info/warn/error are always printed to stderr

use serde::Serialize;
use serde_json::Value;

/// Check if debug logging is enabled (PREDICT_DEBUG=1 or PREDICT_DEBUG=true).
pub fn is_debug() -> bool {
    matches!(
        std::env::var("PREDICT_DEBUG").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Log to stderr at debug level (only if PREDICT_DEBUG=1).
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::output::is_debug() {
            eprintln!("[predict-agent DEBUG] {}", format!($($arg)*));
        }
    };
}

/// Log to stderr at info level (always shown).
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[predict-agent] {}", format!($($arg)*));
    };
}

/// Log to stderr at warn level (always shown).
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        eprintln!("[predict-agent WARN] {}", format!($($arg)*));
    };
}

/// Log to stderr at error level (always shown).
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        eprintln!("[predict-agent ERROR] {}", format!($($arg)*));
    };
}

#[derive(Serialize)]
pub struct Output {
    pub ok: bool,
    pub user_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
    pub _internal: Internal,
}

#[derive(Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub category: String,
    pub retryable: bool,
    pub suggestion: String,
    /// Extra diagnostic details for debugging (URLs tried, raw responses, timing, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug: Option<Value>,
}

/// A choice option for user selection (e.g., persona, mode)
#[derive(Serialize, Clone)]
pub struct Choice {
    pub key: String,
    pub label: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Serialize, Default)]
pub struct Internal {
    pub next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submittable_markets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    /// Progress indicator (e.g., "2/4")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// User choices when multiple options are available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<Choice>>,
}

impl Output {
    pub fn success(user_message: impl Into<String>, data: Value, internal: Internal) -> Self {
        Self {
            ok: true,
            user_message: user_message.into(),
            data: Some(data),
            error: None,
            _internal: internal,
        }
    }

    pub fn error(
        user_message: impl Into<String>,
        code: impl Into<String>,
        category: impl Into<String>,
        retryable: bool,
        suggestion: impl Into<String>,
        internal: Internal,
    ) -> Self {
        Self {
            ok: false,
            user_message: user_message.into(),
            data: None,
            error: Some(ErrorDetail {
                code: code.into(),
                category: category.into(),
                retryable,
                suggestion: suggestion.into(),
                debug: None,
            }),
            _internal: internal,
        }
    }

    /// Create an error output with additional debug info attached.
    pub fn error_with_debug(
        user_message: impl Into<String>,
        code: impl Into<String>,
        category: impl Into<String>,
        retryable: bool,
        suggestion: impl Into<String>,
        debug: Value,
        internal: Internal,
    ) -> Self {
        Self {
            ok: false,
            user_message: user_message.into(),
            data: None,
            error: Some(ErrorDetail {
                code: code.into(),
                category: category.into(),
                retryable,
                suggestion: suggestion.into(),
                debug: Some(debug),
            }),
            _internal: internal,
        }
    }

    pub fn print(&self) {
        println!("{}", serde_json::to_string_pretty(self).unwrap());
    }
}
