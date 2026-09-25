use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::{ChatFile, ChatSnapshot, NotificationState};

const AWAIT_GRACE_MS: u64 = 20_000;

#[derive(Default)]
pub(super) struct AgentWaiting {
    generation: u64,
    until_ms: Option<u64>,
}

struct WaitGuard<'a> {
    chat: &'a ChatFile,
    generation: u64,
    timed_out: bool,
}

impl WaitGuard<'_> {
    fn timed_out(&mut self) {
        let mut state = self
            .chat
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.generation == self.generation {
            state.until_ms = Some(super::now_ms().saturating_add(AWAIT_GRACE_MS));
        }
        self.timed_out = true;
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if !self.timed_out {
            let mut state = self
                .chat
                .waiting
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.generation == self.generation {
                state.until_ms = None;
            }
        }
    }
}

pub enum WaitOutcome {
    Message(ChatSnapshot),
    TimedOut(NotificationState),
    Cancelled,
}

impl ChatFile {
    fn begin_agent_wait(&self, remaining: Duration) -> WaitGuard<'_> {
        let mut state = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        // A disconnected UI must eventually discard the overlay even if the server disappears.
        state.until_ms = Some(
            super::now_ms()
                .saturating_add(remaining.as_millis().min(u64::MAX as u128) as u64)
                .saturating_add(AWAIT_GRACE_MS),
        );
        WaitGuard {
            chat: self,
            generation: state.generation,
            timed_out: false,
        }
    }

    pub(crate) fn clear_agent_waiting(&self) {
        let mut state = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.until_ms = None;
    }

    pub(crate) fn agent_waiting_until_ms(&self) -> Option<u64> {
        self.waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .until_ms
            .filter(|until| *until > super::now_ms())
    }

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
        let mut waiting = self.begin_agent_wait(timeout);
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
                waiting.timed_out();
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

    #[test]
    fn waiting_deadline_tracks_the_actual_wait_duration() {
        let root = tempfile::tempdir().unwrap();
        let chat = ChatFile::new(root.path().join("CHAT.md"), false);
        for duration in [17, 30_000, 300_000] {
            let before = crate::markdown_chat::now_ms();
            let guard = chat.begin_agent_wait(Duration::from_millis(duration));
            let until = chat.agent_waiting_until_ms().unwrap();
            let after = crate::markdown_chat::now_ms();
            assert!(
                (before + duration + AWAIT_GRACE_MS..=after + duration + AWAIT_GRACE_MS)
                    .contains(&until)
            );
            drop(guard);
            assert!(chat.agent_waiting_until_ms().is_none());
        }
    }

    #[test]
    fn stale_wait_completion_cannot_clear_or_revive_a_newer_state() {
        let root = tempfile::tempdir().unwrap();
        let chat = ChatFile::new(root.path().join("CHAT.md"), false);
        let first = chat.begin_agent_wait(Duration::from_secs(1));
        let mut second = chat.begin_agent_wait(Duration::from_secs(5));
        let current = chat.agent_waiting_until_ms();
        drop(first);
        assert_eq!(chat.agent_waiting_until_ms(), current);
        chat.clear_agent_waiting();
        second.timed_out();
        drop(second);
        assert!(chat.agent_waiting_until_ms().is_none());
        let mut third = chat.begin_agent_wait(Duration::ZERO);
        third.timed_out();
        drop(third);
        assert!(chat.agent_waiting_until_ms().is_some());
        chat.waiting.lock().unwrap().until_ms =
            Some(crate::markdown_chat::now_ms().saturating_sub(1));
        assert!(chat.agent_waiting_until_ms().is_none());
    }

    #[tokio::test]
    async fn waiting_is_cleared_by_reply_cancellation_and_dropped_future() {
        for outcome in ["reply", "cancel", "drop", "manual"] {
            let root = tempfile::tempdir().unwrap();
            let chat = Arc::new(ChatFile::new(root.path().join("chat/CHAT.md"), false));
            chat.ensure().await.unwrap();
            assert!(
                chat.owner_summary()
                    .await
                    .unwrap()
                    .agent_waiting_until_ms
                    .is_none()
            );
            let cancellation = CancellationToken::new();
            let task = tokio::spawn({
                let chat = chat.clone();
                let cancellation = cancellation.clone();
                async move {
                    chat.wait_with_watcher(Duration::from_secs(30), cancellation, false)
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while chat.agent_waiting_until_ms().is_none() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                chat.owner_summary()
                    .await
                    .unwrap()
                    .agent_waiting_until_ms
                    .is_some()
            );
            match outcome {
                "reply" => {
                    chat.append_user("reply".into(), "Continue".into())
                        .await
                        .unwrap();
                }
                "manual" => {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(chat.path())
                        .unwrap()
                        .write_all(b"Editor reply\n")
                        .unwrap();
                }
                "cancel" => cancellation.cancel(),
                _ => task.abort(),
            }
            match task.await {
                Ok(Ok(WaitOutcome::Message(_))) => assert!(matches!(outcome, "reply" | "manual")),
                Ok(Ok(WaitOutcome::Cancelled)) => assert_eq!(outcome, "cancel"),
                Err(error) => {
                    assert_eq!(outcome, "drop");
                    assert!(error.is_cancelled());
                }
                _ => panic!("unexpected await outcome"),
            }
            assert!(chat.agent_waiting_until_ms().is_none());
            assert!(
                chat.owner_summary()
                    .await
                    .unwrap()
                    .agent_waiting_until_ms
                    .is_none()
            );
        }
    }

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
