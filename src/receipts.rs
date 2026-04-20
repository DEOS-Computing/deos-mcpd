use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Permit {
        id: String,
        session_id: String,
        upstream: String,
        tool_name: String,
        args_hash: String,
        request_id: String,
        rule_id: String,
        timestamp_ms: u64,
    },
    Receipt {
        id: String,
        permit_id: String,
        session_id: String,
        status: String,
        result_hash: String,
        duration_ms: u64,
        timestamp_ms: u64,
    },
    Denial {
        id: String,
        session_id: String,
        upstream: String,
        tool_name: String,
        args_hash: String,
        request_id: String,
        rule_id: String,
        reason: String,
        timestamp_ms: u64,
    },
    ApprovalRequested {
        id: String,
        session_id: String,
        upstream: String,
        tool_name: String,
        args_hash: String,
        request_id: String,
        rule_id: String,
        timestamp_ms: u64,
    },
    ApprovalResolved {
        id: String,
        pending_id: String,
        session_id: String,
        decision: String,
        timestamp_ms: u64,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn hash_value(v: &Value) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

pub fn permit_id(
    session_id: &str,
    tool_name: &str,
    args_hash: &str,
    request_id: &str,
    ts: u64,
) -> String {
    let canon = serde_json::json!({
        "session_id": session_id,
        "tool_name": tool_name,
        "args_hash": args_hash,
        "request_id": request_id,
        "timestamp_ms": ts,
    });
    hash_value(&canon)
}

pub fn receipt_id(permit_id: &str, result_hash: &str, status: &str, ts: u64) -> String {
    let canon = serde_json::json!({
        "permit_id": permit_id,
        "result_hash": result_hash,
        "status": status,
        "timestamp_ms": ts,
    });
    hash_value(&canon)
}

pub fn denial_id(
    session_id: &str,
    tool_name: &str,
    args_hash: &str,
    request_id: &str,
    rule_id: &str,
    ts: u64,
) -> String {
    let canon = serde_json::json!({
        "kind": "denial",
        "session_id": session_id,
        "tool_name": tool_name,
        "args_hash": args_hash,
        "request_id": request_id,
        "rule_id": rule_id,
        "timestamp_ms": ts,
    });
    hash_value(&canon)
}

pub fn pending_id(
    session_id: &str,
    tool_name: &str,
    args_hash: &str,
    request_id: &str,
    rule_id: &str,
    ts: u64,
) -> String {
    let canon = serde_json::json!({
        "kind": "pending",
        "session_id": session_id,
        "tool_name": tool_name,
        "args_hash": args_hash,
        "request_id": request_id,
        "rule_id": rule_id,
        "timestamp_ms": ts,
    });
    hash_value(&canon)
}

pub fn resolution_id(pending_id: &str, decision: &str, ts: u64) -> String {
    let canon = serde_json::json!({
        "pending_id": pending_id,
        "decision": decision,
        "timestamp_ms": ts,
    });
    hash_value(&canon)
}

#[derive(Clone)]
pub struct RemoteSink {
    pub endpoint: String,
    pub api_key: String,
}

pub struct WriterHandle {
    pub tx: mpsc::Sender<Record>,
    pub join: tokio::task::JoinHandle<()>,
}

pub fn spawn_writer(path: PathBuf, remote: Option<RemoteSink>) -> WriterHandle {
    let (tx, mut rx) = mpsc::channel::<Record>(512);

    // Optional upstream push: batch records, POST every 500ms or 50 records.
    let push_setup = remote.as_ref().map(|_| {
        let (push_tx, push_rx) = mpsc::channel::<Record>(512);
        let remote_clone = remote.clone().unwrap();
        let handle = tokio::spawn(async move {
            run_remote_push(remote_clone, push_rx).await;
        });
        (push_tx, handle)
    });

    let join = tokio::spawn(async move {
        let (remote_tx, push_handle) = match push_setup {
            Some((tx, h)) => (Some(tx), Some(h)),
            None => (None, None),
        };

        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[deos-mcpd] cannot open receipts file {:?}: {}", path, e);
                return;
            }
        };
        while let Some(rec) = rx.recv().await {
            // Local durability first — JSONL is authoritative.
            let mut line = match serde_json::to_vec(&rec) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[deos-mcpd] serialize error: {}", e);
                    continue;
                }
            };
            line.push(b'\n');
            if let Err(e) = file.write_all(&line).await {
                eprintln!("[deos-mcpd] write error: {}", e);
            }
            let _ = file.flush().await;

            // Then fan out to the remote sink. try_send — never block the hot
            // path on a slow receiver; if the push channel is full we drop
            // this push (the record is safe in the local JSONL).
            if let Some(ref tx) = remote_tx {
                if let Err(e) = tx.try_send(rec) {
                    eprintln!("[deos-mcpd] remote push channel full: {}", e);
                }
            }
        }

        // Graceful shutdown: close the push channel and await the push task's
        // final flush before this writer task returns. Without this, the
        // tokio runtime may cancel the push task mid-HTTP-request.
        drop(remote_tx);
        if let Some(handle) = push_handle {
            let _ = handle.await;
        }
    });
    WriterHandle { tx, join }
}

async fn run_remote_push(sink: RemoteSink, mut rx: mpsc::Receiver<Record>) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client");
    let url = format!("{}/v1/receipts", sink.endpoint.trim_end_matches('/'));

    let flush_interval = std::time::Duration::from_millis(500);
    let max_batch = 50usize;
    let mut batch: Vec<Record> = Vec::with_capacity(max_batch);
    let mut ticker = tokio::time::interval(flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe_rec = rx.recv() => {
                match maybe_rec {
                    Some(rec) => {
                        batch.push(rec);
                        if batch.len() >= max_batch {
                            flush_batch(&client, &url, &sink.api_key, &mut batch).await;
                        }
                    }
                    None => {
                        // Channel closed — flush remaining then exit.
                        if !batch.is_empty() {
                            flush_batch(&client, &url, &sink.api_key, &mut batch).await;
                        }
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    flush_batch(&client, &url, &sink.api_key, &mut batch).await;
                }
            }
        }
    }
}

async fn flush_batch(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    batch: &mut Vec<Record>,
) {
    let payload = serde_json::json!({ "records": batch });
    let res = client
        .post(url)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {
            batch.clear();
        }
        Ok(r) => {
            eprintln!(
                "[deos-mcpd] remote push {} — {}",
                r.status(),
                r.text().await.unwrap_or_default()
            );
            // Keep the batch; try again next tick. Bound by channel backpressure.
            if batch.len() > 500 {
                eprintln!("[deos-mcpd] dropping {} records — remote backlog too large", batch.len());
                batch.clear();
            }
        }
        Err(e) => {
            eprintln!("[deos-mcpd] remote push error: {}", e);
            if batch.len() > 500 {
                batch.clear();
            }
        }
    }
}

pub fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub fn emit(tx: &mpsc::Sender<Record>, rec: Record) -> Result<()> {
    if let Err(e) = tx.try_send(rec) {
        eprintln!("[deos-mcpd] receipt channel full: {}", e);
    }
    Ok(())
}
