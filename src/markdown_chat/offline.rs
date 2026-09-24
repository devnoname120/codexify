//! Server-side offline transition notifications for active chat channels.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::{ChatFile, NotificationState, now_ms};

pub(super) const MESSAGE: &str = "Agent status is offline: no Codexify tool call has been recorded for this conversation for 5 minutes.";

pub(super) async fn monitor<F, Fut>(
    chat: Arc<ChatFile>,
    wake: Arc<Notify>,
    offline_after_ms: u64,
    send: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = NotificationState>,
{
    loop {
        let activity = match chat.offline_activity().await {
            Ok(activity) => activity,
            Err(error) => {
                tracing::warn!(%error, "could not inspect agent chat offline activity");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {},
                    _ = wake.notified() => {},
                }
                continue;
            }
        };
        let Some(last_call_at_ms) = activity.last_call_at_ms else {
            wake.notified().await;
            continue;
        };
        if activity.notified_for_tool_call_count == Some(activity.tool_call_count) {
            wake.notified().await;
            continue;
        }

        let remaining_ms = last_call_at_ms
            .saturating_add(offline_after_ms)
            .saturating_sub(now_ms());
        if remaining_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(remaining_ms.min(30_000))) => {},
                _ = wake.notified() => {},
            }
            continue;
        }

        match chat
            .claim_offline_notification(now_ms(), offline_after_ms)
            .await
        {
            Ok(true) => {
                // Persist the claim before delivery: a retry or restart cannot
                // duplicate an accepted provider submission whose response was lost.
                let outcome = send().await;
                tracing::info!(?outcome, "agent chat offline notification attempted");
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "could not claim agent chat offline notification");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {},
                    _ = wake.notified() => {},
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn offline_claim_is_exactly_once_per_last_call_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("chats/conversation/CHAT.md");
        let chat = Arc::new(ChatFile::new(path.clone(), true));
        chat.ensure().await.unwrap();
        assert!(!chat.claim_offline_notification(1600, 600).await.unwrap());

        chat.record_agent_call(1000).await.unwrap();
        assert!(!chat.claim_offline_notification(1599, 600).await.unwrap());
        assert!(chat.claim_offline_notification(1600, 600).await.unwrap());
        assert!(!chat.claim_offline_notification(9999, 600).await.unwrap());

        let reopened = Arc::new(ChatFile::new(path, true));
        assert!(
            !reopened
                .claim_offline_notification(9999, 600)
                .await
                .unwrap()
        );
        reopened.record_agent_call(10_000).await.unwrap();
        assert!(
            !reopened
                .claim_offline_notification(10_599, 600)
                .await
                .unwrap()
        );
        assert!(
            reopened
                .claim_offline_notification(10_600, 600)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn monitor_sends_once_when_offline_and_rearms_after_activity() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chat/CHAT.md"), true));
        chat.record_agent_call(now_ms()).await.unwrap();
        let wake = Arc::new(Notify::new());
        let sends = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(Notify::new());
        let task = tokio::spawn(monitor(chat.clone(), wake.clone(), 50, {
            let sends = sends.clone();
            let delivered = delivered.clone();
            move || {
                let sends = sends.clone();
                let delivered = delivered.clone();
                async move {
                    sends.fetch_add(1, Ordering::SeqCst);
                    delivered.notify_one();
                    NotificationState::Accepted
                }
            }
        }));

        tokio::time::timeout(Duration::from_secs(2), delivered.notified())
            .await
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        wake.notify_one();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);

        chat.record_agent_call(now_ms()).await.unwrap();
        wake.notify_one();
        tokio::time::timeout(Duration::from_secs(2), delivered.notified())
            .await
            .unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 2);
        task.abort();
    }
}
