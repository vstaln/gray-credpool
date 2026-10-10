//! `gray-credpool` host-side CLI — also what `gray credpool <verb>`
//! dispatches to via `command/run`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "gray credpool", about = "Credential pool: multi-key rotation/failover for gray providers")]
pub struct Cli {
    /// Pool file (default: $GRAY_HOME/credpool.json or ~/.gray/credpool.json).
    #[arg(long, global = true, env = "CREDPOOL_FILE")]
    pub file: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Add a key reference to a pool.
    Add {
        provider: String,
        /// env:VAR | file:PATH | cmd:PROG [ARGS…] | auth:NAME
        key_ref: String,
        #[arg(long)]
        label: Option<String>,
        /// Upstream base URL these keys authenticate (sets it when new).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Remove an entry by id or label.
    Remove { provider: String, id: String },
    /// List pools and entries (alias of `status`).
    List { provider: Option<String> },
    /// Pool state: cold/hot keys, cooldowns, next-available time.
    Status { provider: Option<String> },
    /// Bench a key (mark hot) after a failure.
    Mark {
        provider: String,
        /// Entry id/label; default: last-selected key.
        #[arg(long)]
        id: Option<String>,
        /// HTTP status that failed.
        #[arg(long)]
        status: Option<u16>,
        /// billing | rate_limit | auth | error
        #[arg(long)]
        reason: Option<String>,
        /// Cooldown override in seconds.
        #[arg(long)]
        seconds: Option<f64>,
        /// Mark permanently dead until `clear`.
        #[arg(long)]
        dead: bool,
    },
    /// Demote the current pick to the back of the order.
    Rotate { provider: String },
    /// Lift cooldowns (`--id` for one entry, omit for the pool).
    Clear { provider: String, #[arg(long)] id: Option<String> },
    /// Set the default pool for provider/chat + `/credpool`.
    Use { provider: String },
    /// Print the key that would be selected (`--reveal` prints the material).
    Resolve { provider: String, #[arg(long)] reveal: bool },
    /// Emit `export VAR='<selected key>'` for wrapper scripts.
    Env {
        provider: String,
        /// Variable to export (default: OPENAI_API_KEY).
        #[arg(long, default_value = "OPENAI_API_KEY")]
        var: String,
    },
    /// Write the selected key into auth.json / config.json (api_key shim).
    Apply {
        provider: String,
        /// Also set config.json's api_key + credential_source="".
        #[arg(long)]
        activate: bool,
    },
    /// Pool configuration.
    Config {
        provider: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        strategy: Option<String>,
        /// Observe tool/after for upstream-looking failures.
        #[arg(long)]
        watch_tool_errors: Option<bool>,
        /// Comma-separated static model list.
        #[arg(long, value_delimiter = ',')]
        models: Option<Vec<String>>,
    },
    /// Walk the pool against the upstream (or mock endpoint) once.
    Request {
        provider: String,
        #[arg(long, default_value = "POST")]
        method: String,
        #[arg(long, default_value = "/responses")]
        path: String,
        #[arg(long)]
        body: Option<String>,
    },
    /// List models the pool serves (static list or GET {base}/models).
    Models { provider: String },
    /// Copy an existing auth.json key into a pool as auth:<name>.
    Setup {
        provider: String,
        /// auth.json entry name to import.
        #[arg(long)]
        from_auth: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Print the plugin manifest (used by `gray plugin install`).
    Manifest,
    /// Run the stdio sidecar (how gray hosts the plugin).
    Sidecar,
}

pub fn file(cli_file: &Option<PathBuf>) -> Result<PathBuf, String> {
    match cli_file {
        Some(f) => Ok(f.clone()),
        None => crate::pool_path().map_err(|e| e.to_string()),
    }
}

pub fn run(cli: Cli) -> Result<String, String> {
    let path = file(&cli.file)?;
    match cli.cmd {
        Cmd::Add { provider, key_ref, label, base_url } =>
            crate::ops::add(&path, &provider, &key_ref, label.as_deref(), base_url.as_deref()),
        Cmd::Remove { provider, id } => crate::ops::remove(&path, &provider, &id),
        Cmd::List { provider } | Cmd::Status { provider } =>
            crate::ops::status(&path, provider.as_deref()),
        Cmd::Mark { provider, id, status, reason, seconds, dead } =>
            crate::ops::mark(&path, &provider, crate::ops::MarkOpts { id, status, reason, seconds, dead }),
        Cmd::Rotate { provider } => crate::ops::rotate(&path, &provider),
        Cmd::Clear { provider, id } => crate::ops::clear(&path, &provider, id.as_deref()),
        Cmd::Use { provider } => crate::ops::use_pool(&path, &provider),
        Cmd::Resolve { provider, reveal } => crate::ops::resolve(&path, &provider, reveal),
        Cmd::Env { provider, var } => crate::ops::env_line(&path, &provider, &var),
        Cmd::Apply { provider, activate } => crate::ops::apply(&path, &provider, activate),
        Cmd::Config { provider, base_url, strategy, watch_tool_errors, models } =>
            crate::ops::config_set(&path, &provider, base_url.as_deref(), strategy.as_deref(), watch_tool_errors, models),
        Cmd::Request { provider, method, path: up, body } =>
            crate::ops::request(&path, &provider, &method, &up, body.as_deref().map(str::as_bytes)),
        Cmd::Models { provider } => {
            let rows = crate::ops::models(&path, &provider)?;
            Ok(rows.iter().filter_map(|r| r.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect::<Vec<_>>().join("\n"))
        }
        Cmd::Setup { provider, from_auth, label, base_url } => {
            crate::ops::add(&path, &provider, &format!("auth:{from_auth}"), label.as_deref(), base_url.as_deref())
        }
        Cmd::Manifest => Ok(crate::manifest::manifest().to_string()),
        Cmd::Sidecar => Err("sidecar runs on stdio".into()), // handled in main.rs
    }
}
