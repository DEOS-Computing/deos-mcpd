use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::approval::{Approvals, Decision};

#[derive(Clone)]
struct AppState {
    approvals: Arc<Approvals>,
    receipts_path: PathBuf,
}

const DASHBOARD_HTML: &str = include_str!("../dashboard/index.html");

pub async fn run(
    listener: tokio::net::TcpListener,
    approvals: Arc<Approvals>,
    receipts_path: PathBuf,
) -> anyhow::Result<()> {
    let state = AppState {
        approvals,
        receipts_path,
    };
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/pending", get(list_pending))
        .route("/approve/:id", post(approve))
        .route("/deny/:id", post(deny))
        .route("/api/sessions", get(list_sessions))
        .route("/api/records", get(list_records))
        .with_state(state);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn dashboard() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(DASHBOARD_HTML),
    )
}

#[derive(Serialize)]
struct PendingBody {
    pending: Vec<crate::approval::Pending>,
}

async fn list_pending(State(state): State<AppState>) -> Json<PendingBody> {
    Json(PendingBody {
        pending: state.approvals.list().await,
    })
}

async fn approve(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if state.approvals.decide(&id, Decision::Approved).await {
        (StatusCode::OK, "approved").into_response()
    } else {
        (StatusCode::NOT_FOUND, "not pending").into_response()
    }
}

async fn deny(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if state.approvals.decide(&id, Decision::Denied).await {
        (StatusCode::OK, "denied").into_response()
    } else {
        (StatusCode::NOT_FOUND, "not pending").into_response()
    }
}

#[derive(Deserialize)]
struct RecordsQuery {
    session: Option<String>,
    limit: Option<usize>,
    kind: Option<String>,
}

async fn list_records(
    Query(q): Query<RecordsQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match read_records(&state.receipts_path).await {
        Ok(mut records) => {
            if let Some(ref s) = q.session {
                records.retain(|r| r.get("session_id").and_then(Value::as_str) == Some(s));
            }
            if let Some(ref k) = q.kind {
                records.retain(|r| r.get("kind").and_then(Value::as_str) == Some(k));
            }
            let limit = q.limit.unwrap_or(500);
            let start = records.len().saturating_sub(limit);
            let slice = records.split_off(start);
            Json(serde_json::json!({ "records": slice })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {}", e),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct SessionSummary {
    session_id: String,
    records: u64,
    permits: u64,
    receipts: u64,
    denials: u64,
    approvals_requested: u64,
    approvals_resolved: u64,
    first_ms: u64,
    last_ms: u64,
}

async fn list_sessions(State(state): State<AppState>) -> impl IntoResponse {
    match read_records(&state.receipts_path).await {
        Ok(records) => {
            let mut by: BTreeMap<String, SessionSummary> = BTreeMap::new();
            for r in &records {
                let sid = r
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>")
                    .to_string();
                let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
                let ts = r
                    .get("timestamp_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let e = by.entry(sid.clone()).or_insert_with(|| SessionSummary {
                    session_id: sid,
                    records: 0,
                    permits: 0,
                    receipts: 0,
                    denials: 0,
                    approvals_requested: 0,
                    approvals_resolved: 0,
                    first_ms: ts,
                    last_ms: ts,
                });
                e.records += 1;
                match kind {
                    "permit" => e.permits += 1,
                    "receipt" => e.receipts += 1,
                    "denial" => e.denials += 1,
                    "approval_requested" => e.approvals_requested += 1,
                    "approval_resolved" => e.approvals_resolved += 1,
                    _ => {}
                }
                if ts < e.first_ms || e.first_ms == 0 {
                    e.first_ms = ts;
                }
                if ts > e.last_ms {
                    e.last_ms = ts;
                }
            }
            let mut sessions: Vec<SessionSummary> = by.into_values().collect();
            sessions.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
            Json(serde_json::json!({ "sessions": sessions })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {}", e),
        )
            .into_response(),
    }
}

async fn read_records(path: &std::path::Path) -> anyhow::Result<Vec<Value>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(f);
    let mut lines = reader.lines();
    let mut out = Vec::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            out.push(v);
        }
    }
    Ok(out)
}
