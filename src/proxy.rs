use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::jsonrpc;
use crate::receipts::{
    self, emit, hash_value, permit_id, receipt_id, request_id_string, Record,
};

struct PendingPermit {
    permit_id: String,
    tool_name: String,
    started_ms: u64,
}

type PendingMap = Arc<Mutex<HashMap<String, PendingPermit>>>;

pub async fn run(
    cmd: &str,
    args: &[String],
    receipts_path: PathBuf,
    session_id: String,
    upstream: String,
) -> Result<()> {
    let writer = receipts::spawn_writer(receipts_path);

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn upstream: {} {:?}", cmd, args))?;

    let child_stdin = child.stdin.take().context("no stdin on upstream")?;
    let child_stdout = child.stdout.take().context("no stdout on upstream")?;

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    // Client → upstream: read proxy stdin, inspect for tools/call, emit permit, forward.
    let writer_a = writer.clone();
    let pending_a = pending.clone();
    let session_a = session_id.clone();
    let upstream_a = upstream.clone();
    let task_in = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut child_stdin = child_stdin;
        let mut line = String::new();
        loop {
            line.clear();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF from client
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[deos-mcpd] stdin read error: {}", e);
                    break;
                }
            };
            let trimmed = line[..n].trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                    let inspected = jsonrpc::inspect(&msg);
                    if jsonrpc::is_tools_call(&inspected) {
                        if let (Some(params), Some(id_val)) = (inspected.params, inspected.id.as_ref())
                        {
                            let tool = jsonrpc::tool_name(params).unwrap_or("<unknown>");
                            let args_v = jsonrpc::tool_arguments(params).cloned().unwrap_or(Value::Null);
                            let args_hash = hash_value(&args_v);
                            let req_id = request_id_string(id_val);
                            let ts = receipts::now_ms();
                            let pid = permit_id(&session_a, tool, &args_hash, &req_id, ts);

                            let permit = Record::Permit {
                                id: pid.clone(),
                                session_id: session_a.clone(),
                                upstream: upstream_a.clone(),
                                tool_name: tool.to_string(),
                                args_hash,
                                request_id: req_id.clone(),
                                timestamp_ms: ts,
                            };
                            let _ = emit(&writer_a, permit);

                            let mut map = pending_a.lock().await;
                            map.insert(
                                req_id,
                                PendingPermit {
                                    permit_id: pid,
                                    tool_name: tool.to_string(),
                                    started_ms: ts,
                                },
                            );
                        }
                    }
                }
            }

            // Forward verbatim — never mutate the wire format.
            if let Err(e) = child_stdin.write_all(line.as_bytes()).await {
                eprintln!("[deos-mcpd] upstream write error: {}", e);
                break;
            }
            if let Err(e) = child_stdin.flush().await {
                eprintln!("[deos-mcpd] upstream flush error: {}", e);
                break;
            }
        }
        // Let upstream know client is gone.
        drop(child_stdin);
    });

    // Upstream → client: read child stdout, inspect responses, emit receipts, forward.
    let writer_b = writer.clone();
    let pending_b = pending.clone();
    let session_b = session_id.clone();
    let task_out = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();
        loop {
            line.clear();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[deos-mcpd] upstream stdout read error: {}", e);
                    break;
                }
            };
            let trimmed = line[..n].trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                    let inspected = jsonrpc::inspect(&msg);
                    if jsonrpc::is_response(&inspected) {
                        if let Some(id_val) = inspected.id.as_ref() {
                            let req_id = request_id_string(id_val);
                            let mut map = pending_b.lock().await;
                            if let Some(pending) = map.remove(&req_id) {
                                let (status, payload) = if let Some(err) = inspected.error {
                                    ("error", err.clone())
                                } else if let Some(res) = inspected.result {
                                    // MCP tool results may carry isError=true inside result.
                                    let is_err = res
                                        .get("isError")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    (if is_err { "tool_error" } else { "ok" }, res.clone())
                                } else {
                                    ("unknown", Value::Null)
                                };
                                let result_hash = hash_value(&payload);
                                let ts = receipts::now_ms();
                                let rid = receipt_id(&pending.permit_id, &result_hash, status, ts);
                                let receipt = Record::Receipt {
                                    id: rid,
                                    permit_id: pending.permit_id,
                                    session_id: session_b.clone(),
                                    status: status.to_string(),
                                    result_hash,
                                    duration_ms: ts.saturating_sub(pending.started_ms),
                                    timestamp_ms: ts,
                                };
                                let _ = emit(&writer_b, receipt);
                                // Suppress unused variable warning for tool_name.
                                let _ = pending.tool_name;
                            }
                        }
                    }
                }
            }
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                eprintln!("[deos-mcpd] client write error: {}", e);
                break;
            }
            if let Err(e) = stdout.flush().await {
                eprintln!("[deos-mcpd] client flush error: {}", e);
                break;
            }
        }
    });

    let _ = tokio::join!(task_in, task_out);
    let status = child.wait().await?;
    if !status.success() {
        eprintln!("[deos-mcpd] upstream exited: {:?}", status);
    }
    Ok(())
}
