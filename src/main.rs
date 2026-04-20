use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod approval;
mod control;
mod jsonrpc;
mod policy;
mod proxy;
mod receipts;

use approval::Approvals;
use policy::Policy;

#[derive(Parser, Debug)]
#[command(
    name = "deos-mcpd",
    version,
    about = "Governance proxy for MCP servers — permits, receipts, policy, approval."
)]
struct Args {
    /// Upstream MCP server command + args. Everything after `--upstream` is
    /// executed as a child process, stdio-piped through this proxy.
    ///
    /// Example: --upstream npx -y @modelcontextprotocol/server-filesystem /tmp
    #[arg(
        long,
        num_args = 1..,
        required = true,
        allow_hyphen_values = true,
        trailing_var_arg = true
    )]
    upstream: Vec<String>,

    /// Path to receipts JSONL file. Defaults to ~/.deos-mcpd/receipts.jsonl.
    #[arg(long)]
    receipts: Option<PathBuf>,

    /// Override session ID. Defaults to a UUID v4 generated at startup.
    #[arg(long)]
    session_id: Option<String>,

    /// Path to YAML policy file. If omitted, a default-allow policy is used.
    #[arg(long)]
    policy: Option<PathBuf>,

    /// Bind address for the control HTTP API (used for approvals + dashboards).
    /// Defaults to 127.0.0.1:4005. Pass "off" to disable.
    #[arg(long, default_value = "127.0.0.1:4005")]
    control: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();

    let receipts_path = match args.receipts {
        Some(p) => p,
        None => default_receipts_path()?,
    };
    let session_id = args
        .session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let policy = match args.policy {
        Some(path) => Policy::load(&path)
            .with_context(|| format!("failed to load policy from {:?}", path))?,
        None => Policy::open_default(),
    };
    let policy = Arc::new(policy);

    let approvals = Approvals::new();

    if args.control.to_ascii_lowercase() != "off" {
        let bind: SocketAddr = args
            .control
            .parse()
            .with_context(|| format!("invalid --control address {:?}", args.control))?;
        let approvals_clone = approvals.clone();
        let receipts_clone = receipts_path.clone();
        tokio::spawn(async move {
            if let Err(e) = control::run(bind, approvals_clone, receipts_clone).await {
                eprintln!("[deos-mcpd] control server error: {}", e);
            }
        });
    }

    let (cmd, cmd_args) = args
        .upstream
        .split_first()
        .context("--upstream requires at least one argument")?;

    let cfg = proxy::ProxyConfig {
        cmd: cmd.to_string(),
        args: cmd_args.to_vec(),
        receipts_path,
        session_id,
        upstream: args.upstream.join(" "),
        policy,
        approvals,
    };
    proxy::run(cfg).await
}

fn default_receipts_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    let dir = home.join(".deos-mcpd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("receipts.jsonl"))
}
