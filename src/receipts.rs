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
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn hash_value(v: &Value) -> String {
    // Canonical-ish: serde_json::to_vec is stable for a given Value.
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

pub fn permit_id(session_id: &str, tool_name: &str, args_hash: &str, request_id: &str, ts: u64) -> String {
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

pub fn spawn_writer(path: PathBuf) -> mpsc::Sender<Record> {
    let (tx, mut rx) = mpsc::channel::<Record>(256);
    tokio::spawn(async move {
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
        }
    });
    tx
}

pub fn request_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub fn emit(tx: &mpsc::Sender<Record>, rec: Record) -> Result<()> {
    // Use blocking try_send — receipts are not on the hot path.
    if let Err(e) = tx.try_send(rec) {
        eprintln!("[deos-mcpd] receipt channel full: {}", e);
    }
    Ok(())
}
