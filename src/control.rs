use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::approval::{Approvals, Decision};

#[derive(Clone)]
struct AppState {
    approvals: Arc<Approvals>,
}

pub async fn run(bind: SocketAddr, approvals: Arc<Approvals>) -> anyhow::Result<()> {
    let state = AppState { approvals };
    let app = Router::new()
        .route("/health", get(health))
        .route("/pending", get(list_pending))
        .route("/approve/:id", post(approve))
        .route("/deny/:id", post(deny))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("[deos-mcpd] control API listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
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
