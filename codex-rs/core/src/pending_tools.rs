use std::collections::HashMap;

use codex_protocol::models::FunctionCallOutputPayload;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq)]
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
        let (metadata, receiver, remove_entry) = {
            let entry = guard.get_mut(call_id)?;
            let receiver = entry.receiver.take()?;
            let metadata = entry.metadata.clone();
            let remove_entry = entry.sender.is_none();
            (metadata, receiver, remove_entry)
        };
        if remove_entry {
            guard.remove(call_id);
        }
        Some((metadata, receiver))
    }

    pub(crate) async fn resolve(
        &self,
        call_id: &str,
        payload: FunctionCallOutputPayload,
    ) -> Option<PendingToolMetadata> {
        let mut guard = self.entries.lock().await;
        let (metadata, remove_entry) = {
            let entry = guard.get_mut(call_id)?;
            if let Some(sender) = entry.sender.take() {
                let _ = sender.send(payload);
            }
            let metadata = entry.metadata.clone();
            let remove_entry = entry.receiver.is_none();
            (metadata, remove_entry)
        };
        if remove_entry {
            guard.remove(call_id);
        }
        Some(metadata)
    }

    pub(crate) async fn cancel(&self, call_id: &str) -> Option<PendingToolMetadata> {
        let mut guard = self.entries.lock().await;
        guard.remove(call_id).map(|entry| entry.metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn payload(text: &str) -> FunctionCallOutputPayload {
        FunctionCallOutputPayload {
            content: text.to_string(),
            content_items: None,
            success: Some(true),
        }
    }

    #[tokio::test]
    async fn resolve_after_taking_receiver_delivers_payload_and_removes_entry() {
        let manager = PendingToolManager::new();
        let meta = manager
            .register(
                "call-1".to_string(),
                "tool".to_string(),
                "turn-1".to_string(),
                None,
            )
            .await;

        let (taken_meta, rx) = manager
            .take_receiver("call-1")
            .await
            .expect("receiver should be available");
        assert_eq!(taken_meta, meta);

        let resolved_meta = manager
            .resolve("call-1", payload("ok"))
            .await
            .expect("resolve should return metadata");
        assert_eq!(resolved_meta, meta);

        assert_eq!(rx.await.expect("payload"), payload("ok"));
        assert_eq!(manager.cancel("call-1").await, None);
    }

    #[tokio::test]
    async fn take_receiver_after_resolve_delivers_payload_and_removes_entry() {
        let manager = PendingToolManager::new();
        let meta = manager
            .register(
                "call-1".to_string(),
                "tool".to_string(),
                "turn-1".to_string(),
                None,
            )
            .await;

        let resolved_meta = manager
            .resolve("call-1", payload("ok"))
            .await
            .expect("resolve should return metadata");
        assert_eq!(resolved_meta, meta);

        let (taken_meta, rx) = manager
            .take_receiver("call-1")
            .await
            .expect("receiver should be available");
        assert_eq!(taken_meta, meta);

        assert_eq!(rx.await.expect("payload"), payload("ok"));
        assert_eq!(manager.cancel("call-1").await, None);
    }

    #[tokio::test]
    async fn cancel_removes_entry_without_delivering() {
        let manager = PendingToolManager::new();
        let meta = manager
            .register(
                "call-1".to_string(),
                "tool".to_string(),
                "turn-1".to_string(),
                None,
            )
            .await;

        assert_eq!(manager.cancel("call-1").await, Some(meta));
        assert!(manager.take_receiver("call-1").await.is_none());
        assert_eq!(manager.resolve("call-1", payload("ok")).await, None);
    }
}
