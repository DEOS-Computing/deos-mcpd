use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::approval::{Approvals, Decision, Pending};
use crate::jsonrpc;
use crate::policy::{Action, Policy};
use crate::receipts::{
    self, denial_id, emit, hash_value, pending_id, permit_id, receipt_id, request_id_string,
    resolution_id, Record,
};

struct InFlight {
    permit_id: String,
    started_ms: u64,
}

type PendingMap = Arc<Mutex<HashMap<String, InFlight>>>;

pub struct ProxyConfig {
    pub cmd: String,
    pub args: Vec<String>,
    pub receipts_path: PathBuf,
    pub session_id: String,
    pub upstream: String,
    pub policy: Arc<Policy>,
    pub approvals: Arc<Approvals>,
    pub remote: Option<crate::receipts::RemoteSink>,
}

pub async fn run(cfg: ProxyConfig) -> Result<()> {
    let writer_handle = receipts::spawn_writer(cfg.receipts_path, cfg.remote.clone());
    let writer = writer_handle.tx;
    let writer_join = writer_handle.join;

    let mut child = Command::new(&cfg.cmd)
        .args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn upstream: {} {:?}", cfg.cmd, cfg.args))?;

    let child_stdin = child.stdin.take().context("no stdin on upstream")?;
    let child_stdout = child.stdout.take().context("no stdout on upstream")?;

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let approval_tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // Dedicated writer tasks: single owner of client stdout and upstream stdin.
    let (client_tx, mut client_rx) = mpsc::channel::<Vec<u8>>(256);
    let (upstream_tx, mut upstream_rx) = mpsc::channel::<Vec<u8>>(256);

    let client_writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(bytes) = client_rx.recv().await {
            if stdout.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let upstream_writer = tokio::spawn(async move {
        let mut child_stdin = child_stdin;
        while let Some(bytes) = upstream_rx.recv().await {
            if child_stdin.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = child_stdin.flush().await;
        }
    });

    // Upstream → client: inspect responses, emit receipts, forward.
    let writer_b = writer.clone();
    let pending_b = pending.clone();
    let session_b = cfg.session_id.clone();
    let client_tx_b = client_tx.clone();
    let task_out = tokio::spawn(async move {
        let mut reader = BufReader::new(child_stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[deos-mcpd] upstream stdout error: {}", e);
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
                                let _ = emit(
                                    &writer_b,
                                    Record::Receipt {
                                        id: rid,
                                        permit_id: pending.permit_id,
                                        session_id: session_b.clone(),
                                        status: status.to_string(),
                                        result_hash,
                                        duration_ms: ts.saturating_sub(pending.started_ms),
                                        timestamp_ms: ts,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            if client_tx_b.send(line.as_bytes().to_vec()).await.is_err() {
                break;
            }
        }
    });

    // Client → proxy: inspect tools/call, evaluate policy, dispatch.
    let writer_a = writer.clone();
    let pending_a = pending.clone();
    let session_a = cfg.session_id.clone();
    let upstream_a = cfg.upstream.clone();
    let policy_a = cfg.policy.clone();
    let approvals_a = cfg.approvals.clone();
    let approval_tasks_a = approval_tasks.clone();
    let client_tx_a = client_tx.clone();
    let upstream_tx_a = upstream_tx.clone();
    let task_in = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[deos-mcpd] stdin read error: {}", e);
                    break;
                }
            };
            let mut to_forward: Option<Vec<u8>> = Some(line[..n].as_bytes().to_vec());
            let trimmed = line[..n].trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                    let inspected = jsonrpc::inspect(&msg);
                    if jsonrpc::is_tools_call(&inspected) {
                        if let (Some(params), Some(id_val)) =
                            (inspected.params, inspected.id.as_ref())
                        {
                            let tool = jsonrpc::tool_name(params).unwrap_or("<unknown>").to_string();
                            let args = jsonrpc::tool_arguments(params)
                                .cloned()
                                .unwrap_or(Value::Null);
                            let args_hash = hash_value(&args);
                            let req_id = request_id_string(id_val);
                            let ts = receipts::now_ms();

                            let verdict = policy_a.evaluate(&tool, &args);
                            let rule_id = verdict.rule_id().to_string();

                            match verdict.action {
                                Action::Allow => {
                                    let pid =
                                        permit_id(&session_a, &tool, &args_hash, &req_id, ts);
                                    let _ = emit(
                                        &writer_a,
                                        Record::Permit {
                                            id: pid.clone(),
                                            session_id: session_a.clone(),
                                            upstream: upstream_a.clone(),
                                            tool_name: tool.clone(),
                                            args_hash: args_hash.clone(),
                                            request_id: req_id.clone(),
                                            rule_id,
                                            timestamp_ms: ts,
                                        },
                                    );
                                    pending_a.lock().await.insert(
                                        req_id,
                                        InFlight {
                                            permit_id: pid,
                                            started_ms: ts,
                                        },
                                    );
                                    if let Some(raw) = to_forward.take() {
                                        let _ = upstream_tx_a.send(raw).await;
                                    }
                                }
                                Action::Deny => {
                                    let did = denial_id(
                                        &session_a,
                                        &tool,
                                        &args_hash,
                                        &req_id,
                                        &rule_id,
                                        ts,
                                    );
                                    let reason = format!(
                                        "blocked by deos-mcpd policy rule {}",
                                        rule_id
                                    );
                                    let _ = emit(
                                        &writer_a,
                                        Record::Denial {
                                            id: did,
                                            session_id: session_a.clone(),
                                            upstream: upstream_a.clone(),
                                            tool_name: tool.clone(),
                                            args_hash: args_hash.clone(),
                                            request_id: req_id.clone(),
                                            rule_id: rule_id.clone(),
                                            reason: reason.clone(),
                                            timestamp_ms: ts,
                                        },
                                    );
                                    let synth = synth_error(id_val, -32001, &reason, &rule_id);
                                    let _ = client_tx_a.send(synth).await;
                                    to_forward = None;
                                }
                                Action::RequireApproval => {
                                    let pending_hash = pending_id(
                                        &session_a,
                                        &tool,
                                        &args_hash,
                                        &req_id,
                                        &rule_id,
                                        ts,
                                    );
                                    let _ = emit(
                                        &writer_a,
                                        Record::ApprovalRequested {
                                            id: pending_hash.clone(),
                                            session_id: session_a.clone(),
                                            upstream: upstream_a.clone(),
                                            tool_name: tool.clone(),
                                            args_hash: args_hash.clone(),
                                            request_id: req_id.clone(),
                                            rule_id: rule_id.clone(),
                                            timestamp_ms: ts,
                                        },
                                    );
                                    let rx = approvals_a
                                        .register(Pending {
                                            id: pending_hash.clone(),
                                            session_id: session_a.clone(),
                                            rule_id: rule_id.clone(),
                                            tool_name: tool.clone(),
                                            arguments: args.clone(),
                                            upstream: upstream_a.clone(),
                                            created_ms: ts,
                                        })
                                        .await;

                                    let timeout = verdict.approval_timeout();
                                    let writer_c = writer_a.clone();
                                    let pending_c = pending_a.clone();
                                    let session_c = session_a.clone();
                                    let upstream_c = upstream_a.clone();
                                    let client_tx_c = client_tx_a.clone();
                                    let upstream_tx_c = upstream_tx_a.clone();
                                    let approvals_c = approvals_a.clone();
                                    let id_owned = id_val.clone();
                                    let held_line = to_forward.take().unwrap_or_default();
                                    let tool_c = tool.clone();
                                    let args_hash_c = args_hash.clone();
                                    let req_id_c = req_id.clone();
                                    let rule_id_c = rule_id.clone();

                                    let approval_tasks_clone = approval_tasks_a.clone();
                                    let handle = tokio::spawn(async move {
                                        let outcome =
                                            match tokio::time::timeout(timeout, rx).await {
                                                Ok(Ok(d)) => d,
                                                Ok(Err(_)) => Decision::TimedOut,
                                                Err(_) => Decision::TimedOut,
                                            };
                                        let done_ts = receipts::now_ms();
                                        let decision_str = match outcome {
                                            Decision::Approved => "approved",
                                            Decision::Denied => "denied",
                                            Decision::TimedOut => "timed_out",
                                        };
                                        let rid = resolution_id(
                                            &pending_hash,
                                            decision_str,
                                            done_ts,
                                        );
                                        let _ = emit(
                                            &writer_c,
                                            Record::ApprovalResolved {
                                                id: rid,
                                                pending_id: pending_hash.clone(),
                                                session_id: session_c.clone(),
                                                decision: decision_str.to_string(),
                                                timestamp_ms: done_ts,
                                            },
                                        );

                                        match outcome {
                                            Decision::Approved => {
                                                let pid = permit_id(
                                                    &session_c,
                                                    &tool_c,
                                                    &args_hash_c,
                                                    &req_id_c,
                                                    done_ts,
                                                );
                                                let _ = emit(
                                                    &writer_c,
                                                    Record::Permit {
                                                        id: pid.clone(),
                                                        session_id: session_c.clone(),
                                                        upstream: upstream_c.clone(),
                                                        tool_name: tool_c.clone(),
                                                        args_hash: args_hash_c.clone(),
                                                        request_id: req_id_c.clone(),
                                                        rule_id: rule_id_c.clone(),
                                                        timestamp_ms: done_ts,
                                                    },
                                                );
                                                pending_c.lock().await.insert(
                                                    req_id_c,
                                                    InFlight {
                                                        permit_id: pid,
                                                        started_ms: done_ts,
                                                    },
                                                );
                                                let _ = upstream_tx_c.send(held_line).await;
                                            }
                                            Decision::Denied | Decision::TimedOut => {
                                                approvals_c.purge(&pending_hash).await;
                                                let reason = match outcome {
                                                    Decision::Denied => format!(
                                                        "approval denied for rule {}",
                                                        rule_id_c
                                                    ),
                                                    _ => format!(
                                                        "approval timed out for rule {}",
                                                        rule_id_c
                                                    ),
                                                };
                                                let did = denial_id(
                                                    &session_c,
                                                    &tool_c,
                                                    &args_hash_c,
                                                    &req_id_c,
                                                    &rule_id_c,
                                                    done_ts,
                                                );
                                                let _ = emit(
                                                    &writer_c,
                                                    Record::Denial {
                                                        id: did,
                                                        session_id: session_c.clone(),
                                                        upstream: upstream_c.clone(),
                                                        tool_name: tool_c.clone(),
                                                        args_hash: args_hash_c.clone(),
                                                        request_id: req_id_c.clone(),
                                                        rule_id: rule_id_c.clone(),
                                                        reason: reason.clone(),
                                                        timestamp_ms: done_ts,
                                                    },
                                                );
                                                let synth = synth_error(
                                                    &id_owned,
                                                    -32001,
                                                    &reason,
                                                    &rule_id_c,
                                                );
                                                let _ = client_tx_c.send(synth).await;
                                            }
                                        }
                                    });
                                    approval_tasks_clone.lock().await.push(handle);
                                }
                            }
                        }
                    }
                }
            }

            if let Some(raw) = to_forward {
                // Pass through unchanged (initialize, notifications/*, tools/list, etc.)
                if upstream_tx_a.send(raw).await.is_err() {
                    break;
                }
            }
        }
        drop(upstream_tx_a);
    });

    // Wait for stdin EOF (normal Claude Desktop disconnect). Don't wait on
    // task_out here — the upstream MCP server keeps stdout open until *its*
    // stdin closes, which only happens after we drop the upstream_tx sender.
    let _ = task_in.await;

    // Drain pending approvals as denied so the audit trail never has a
    // dangling approval_requested. Waiter tasks see Decision::Denied on
    // their rx and emit ApprovalResolved + Denial via writer_c.
    let drained = cfg.approvals.shutdown_drain().await;
    if !drained.is_empty() {
        eprintln!(
            "[deos-mcpd] shutdown: denying {} pending approval(s)",
            drained.len()
        );
    }
    let handles: Vec<_> = approval_tasks.lock().await.drain(..).collect();
    for h in handles {
        let _ = h.await;
    }

    // Signal upstream writer → closes child stdin → upstream exits → task_out EOFs.
    drop(upstream_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task_out).await;

    drop(client_tx);
    let _ = tokio::join!(client_writer, upstream_writer);
    // Drop the writer sender so the receipts writer task flushes + exits.
    // Then await the writer task so its final HTTP push to the remote
    // receiver completes before we return (otherwise the tokio runtime
    // shutdown can cancel an in-flight reqwest mid-POST).
    drop(writer);
    let _ = writer_join.await;
    let status = child.wait().await?;
    if !status.success() {
        eprintln!("[deos-mcpd] upstream exited: {:?}", status);
    }
    Ok(())
}

fn synth_error(id: &Value, code: i64, message: &str, rule_id: &str) -> Vec<u8> {
    let obj = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": {
                "source": "deos-mcpd",
                "rule_id": rule_id,
            }
        }
    });
    let mut s = serde_json::to_vec(&obj).unwrap_or_default();
    s.push(b'\n');
    s
}
