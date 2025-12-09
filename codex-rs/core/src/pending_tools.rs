use std::collections::HashMap;

use codex_protocol::models::FunctionCallOutputPayload;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub(crate) struct PendingToolMetadata {
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) turn_id: String,
    pub(crate) note: Option<String>,
}

struct PendingToolEntry {
    metadata: PendingToolMetadata,
    receiver: Option<oneshot::Receiver<FunctionCallOutputPayload>>,
    sender: Option<oneshot::Sender<FunctionCallOutputPayload>>,
}

impl PendingToolEntry {
    fn new(metadata: PendingToolMetadata) -> Self {
        let (tx, rx) = oneshot::channel();
        Self {
            metadata,
            receiver: Some(rx),
            sender: Some(tx),
        }
    }
}

pub(crate) struct PendingToolManager {
    entries: Mutex<HashMap<String, PendingToolEntry>>,
}

impl PendingToolManager {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn register(
        &self,
        call_id: String,
        tool_name: String,
        turn_id: String,
        note: Option<String>,
    ) -> PendingToolMetadata {
        let metadata = PendingToolMetadata {
            call_id: call_id.clone(),
            tool_name,
            turn_id,
            note,
        };
        let mut guard = self.entries.lock().await;
        guard.insert(call_id, PendingToolEntry::new(metadata.clone()));
        metadata
    }

    pub(crate) async fn take_receiver(
        &self,
        call_id: &str,
    ) -> Option<(
        PendingToolMetadata,
        oneshot::Receiver<FunctionCallOutputPayload>,
    )> {
        let mut guard = self.entries.lock().await;
        guard
            .get_mut(call_id)
            .and_then(|entry| entry.receiver.take().map(|rx| (entry.metadata.clone(), rx)))
    }

    pub(crate) async fn resolve(
        &self,
        call_id: &str,
        payload: FunctionCallOutputPayload,
    ) -> Option<PendingToolMetadata> {
        let mut guard = self.entries.lock().await;
        guard.remove(call_id).map(|mut entry| {
            if let Some(sender) = entry.sender.take() {
                let _ = sender.send(payload);
            }
            entry.metadata
        })
    }
}
