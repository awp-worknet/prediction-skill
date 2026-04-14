/// predict-agent — CLI for AWP Predict WorkNet.
///
/// Usage: predict-agent <COMMAND> [OPTIONS]
///
/// Environment variables:
///   PREDICT_SERVER_URL   Coordinator URL (default: https://api.agentpredict.work)
///   AWP_ADDRESS          Agent wallet address (for dev/test)
///   AWP_PRIVATE_KEY      Agent private key in hex (for dev/test signing)
///   AWP_DEV_MODE         Set to "true" to use dev signature bypass
///   AWP_WALLET_TOKEN     Session token from awp-wallet (optional, for backward compat)
///   AWP_AGENT_ID         Agent ID for awp-wallet multi-agent support

mod auth;
mod awp_register;
mod client;
mod cmd;
mod output;
mod wallet;

use anyhow::Result;
use clap::{Parser, Subcommand};

const DEFAULT_SERVER: &str = "https://api.agentpredict.work";

#[derive(Parser)]
#[command(
    name = "predict-agent",
    version,
    about = "CLI for AWP Predict WorkNet — submit predictions and earn $PRED",
    long_about = None,
)]
struct Cli {
    /// Coordinator server URL
    #[arg(
        long,
        env = "PREDICT_SERVER_URL",
        default_value = DEFAULT_SERVER,
        global = true
    )]
    server: String,

    /// Output raw JSON (default, always on)
    #[arg(long, global = true, hide = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check all prerequisites (wallet, connectivity, registration)
    Preflight,

    /// Check wallet status and safety (is init safe? is wallet locked?)
    Wallet,

    /// Fetch full decision context: agent status + markets + klines + recommendation
    Context,

    /// Submit a prediction
    Submit {
        /// Market ID (e.g. btc-15m-20260410-1200)
        #[arg(long)]
        market: String,

        /// Direction: up or down
        #[arg(long)]
        prediction: String,

        /// Number of tickets to stake
        #[arg(long)]
        tickets: u32,

        /// Your reasoning (80-2000 characters)
        #[arg(long)]
        reasoning: String,

        /// Optional limit price (0.01-0.99). If omitted, takes best available price.
        #[arg(long)]
        limit_price: Option<f64>,

        /// Preview without submitting
        #[arg(long)]
        dry_run: bool,

        /// Challenge nonce from `predict-agent challenge --market X`
        #[arg(long)]
        challenge_nonce: String,
    },

    /// Fetch an SMHL challenge for a market (nonce + constraints reasoning must satisfy)
    Challenge {
        /// Market ID
        #[arg(long)]
        market: String,
    },

    /// Show current agent status (balance, submissions, etc.)
    Status,

    /// Show the outcome of a specific market
    Result {
        /// Market ID
        #[arg(long)]
        market: String,
    },

    /// Show recent prediction history
    History {
        /// Number of predictions to show
        #[arg(long, default_value = "20")]
        limit: u32,
    },

    /// Set agent persona (7-day cooldown)
    SetPersona {
        /// Persona name
        persona: String,
    },

    /// List your orders (open, filled, cancelled)
    Orders {
        /// Filter by market ID
        #[arg(long)]
        market: Option<String>,

        /// Filter by status: open, filled, cancelled, or all
        #[arg(long, default_value = "all")]
        status: String,

        /// Number of orders to show
        #[arg(long, default_value = "20")]
        limit: u32,
    },

    /// Cancel an open order
    Cancel {
        /// Order ID to cancel
        #[arg(long)]
        order: i64,
    },

    /// [Admin] Trigger chip feed for zero-balance agents
    AdminChipFeed,

    /// Run continuous prediction loop (background worker)
    Loop {
        /// Seconds between prediction rounds
        #[arg(long, default_value = "120")]
        interval: u64,

        /// Max iterations (0 = unlimited)
        #[arg(long, default_value = "0")]
        max_iterations: u64,

        /// OpenClaw agent ID for LLM calls
        #[arg(long, default_value = "predict-worker")]
        agent_id: String,

        /// Output [NOTIFY] lines for each round (agent relays to user)
        #[arg(long)]
        notify: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let server = &cli.server;

    match cli.command {
        Commands::Preflight => cmd::preflight::run(server)?,
        Commands::Wallet => cmd::wallet_check::run()?,
        Commands::Context => cmd::context::run(server)?,
        Commands::Submit {
            market,
            prediction,
            tickets,
            reasoning,
            limit_price,
            dry_run,
            challenge_nonce,
        } => cmd::submit::run(
            server,
            cmd::submit::SubmitArgs {
                market,
                prediction,
                tickets,
                reasoning,
                limit_price,
                dry_run,
                challenge_nonce,
            },
        )?,
        Commands::Challenge { market } => cmd::challenge::run(server, &market)?,
        Commands::Status => cmd::status::run(server)?,
        Commands::Result { market } => cmd::result::run(server, &market)?,
        Commands::History { limit } => cmd::history::run(server, limit)?,
        Commands::SetPersona { persona } => cmd::set_persona::run(server, &persona)?,
        Commands::Orders { market, status, limit } => cmd::orders::run(server, market, &status, limit)?,
        Commands::Cancel { order } => cmd::cancel::run(server, order)?,
        Commands::AdminChipFeed => cmd::admin_chip_feed::run(server)?,
        Commands::Loop {
            interval,
            max_iterations,
            agent_id,
            notify,
        } => cmd::loop_worker::run(
            server,
            cmd::loop_worker::LoopArgs {
                interval,
                max_iterations,
                agent_id,
                notify,
            },
        )?,
    }

    Ok(())
}
