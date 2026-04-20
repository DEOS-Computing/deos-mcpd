use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Denied,
    TimedOut,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub id: String,
    pub session_id: String,
    pub rule_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub upstream: String,
    pub created_ms: u64,
}

struct Waiter {
    info: Pending,
    sender: oneshot::Sender<Decision>,
}

#[derive(Default)]
pub struct Approvals {
    inner: Mutex<HashMap<String, Waiter>>,
}

impl Approvals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn register(
        &self,
        info: Pending,
    ) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        let waiter = Waiter {
            info: info.clone(),
            sender: tx,
        };
        self.inner.lock().await.insert(info.id.clone(), waiter);
        rx
    }

    pub async fn list(&self) -> Vec<Pending> {
        self.inner
            .lock()
            .await
            .values()
            .map(|w| w.info.clone())
            .collect()
    }

    pub async fn decide(&self, id: &str, decision: Decision) -> bool {
        let waiter = self.inner.lock().await.remove(id);
        if let Some(w) = waiter {
            let _ = w.sender.send(decision);
            true
        } else {
            false
        }
    }

    pub async fn purge(&self, id: &str) {
        self.inner.lock().await.remove(id);
    }
}
