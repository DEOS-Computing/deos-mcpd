use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

mod jsonrpc;
mod proxy;
mod receipts;

#[derive(Parser, Debug)]
#[command(
    name = "deos-mcpd",
    version,
    about = "Governance proxy for MCP servers — permits, receipts, audit trail."
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

    let (cmd, cmd_args) = args
        .upstream
        .split_first()
        .context("--upstream requires at least one argument")?;

    proxy::run(
        cmd,
        cmd_args,
        receipts_path,
        session_id,
        args.upstream.join(" "),
    )
    .await
}

fn default_receipts_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    let dir = home.join(".deos-mcpd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("receipts.jsonl"))
}
