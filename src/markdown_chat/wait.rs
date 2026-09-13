use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::{ChatFile, ChatSnapshot, NotificationState};

pub enum WaitOutcome {
    Message(ChatSnapshot),
    TimedOut(NotificationState),
    Cancelled,
}

impl ChatFile {
    pub async fn wait(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<WaitOutcome, String> {
        self.wait_with_watcher(timeout, cancellation, true).await
    }

    async fn wait_with_watcher(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
        native: bool,
    ) -> Result<WaitOutcome, String> {
        let deadline = Instant::now() + timeout;
        if cancellation.is_cancelled() {
            return Ok(WaitOutcome::Cancelled);
        }
        let first = self.read_cancellable(true, cancellation.clone()).await;
        if cancellation.is_cancelled() {
            return Ok(WaitOutcome::Cancelled);
        }
        let first = first?;
        if !first.text.is_empty() {
            return Ok(WaitOutcome::Message(first));
        }

        let (sender, mut events) = mpsc::channel(1);
        let path = std::fs::canonicalize(self.path()).unwrap_or_else(|_| self.path().to_path_buf());
        let mut watcher = if native {
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
                Ok(event)
                    if !matches!(event.kind, EventKind::Access(_))
                        && (event.need_rescan()
                            || event.paths.iter().any(|changed| changed == &path)) =>
                {
                    let _ = sender.try_send(false);
                }
                Err(_) => {
                    let _ = sender.try_send(true);
                }
                _ => {}
            })
            .ok()
        } else {
            None
        };
        if let Some(watching) = watcher.as_mut()
            && watching
                .watch(
                    self.path().parent().ok_or("CHAT.md has no parent")?,
                    RecursiveMode::NonRecursive,
                )
                .is_err()
        {
            watcher = None;
        }

        loop {
            if cancellation.is_cancelled() {
                return Ok(WaitOutcome::Cancelled);
            }
            // Re-read after registration and at the deadline; event delivery is only a wake-up hint.
            let snapshot = self.read_cancellable(true, cancellation.clone()).await;
            if cancellation.is_cancelled() {
                return Ok(WaitOutcome::Cancelled);
            }
            let snapshot = snapshot?;
            if !snapshot.text.is_empty() {
                return Ok(WaitOutcome::Message(snapshot));
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome::TimedOut(snapshot.notification));
            }
            // Some network filesystems accept a native watch but never deliver events.
            let poll = if watcher.is_some() {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(500)
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(WaitOutcome::Cancelled),
                event = events.recv(), if watcher.is_some() => {
                    if event != Some(false) { watcher = None; }
                }
                _ = sleep_until(deadline.min(Instant::now() + poll)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn polling_fallback_returns_the_complete_append() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chat/CHAT.md"), false));
        chat.ensure().await.unwrap();
        let waiting = tokio::spawn({
            let chat = chat.clone();
            async move {
                chat.wait_with_watcher(Duration::from_secs(5), CancellationToken::new(), false)
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::OpenOptions::new()
            .append(true)
            .open(chat.path())
            .unwrap()
            .write_all(b"polling reply\n")
            .unwrap();
        match waiting.await.unwrap().unwrap() {
            WaitOutcome::Message(snapshot) => assert_eq!(snapshot.text, "polling reply\n"),
            _ => panic!("fallback missed the append"),
        }
    }
}
