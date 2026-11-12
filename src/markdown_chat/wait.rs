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
    input_required: bool,
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
    /// Wake a running tool for unread owner/team text without acknowledging it,
    /// entering input-required mode, or releasing queued messages.
    pub(crate) async fn wait_for_unread(
        self: &Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), String> {
        self.wait_for_unread_with_watcher(cancellation, true).await
    }

    async fn wait_for_unread_with_watcher(
        self: &Arc<Self>,
        cancellation: CancellationToken,
        native: bool,
    ) -> Result<(), String> {
        if cancellation.is_cancelled() {
            return Err("Owner message observation cancelled".into());
        }
        let unread = self.unread_snapshot().await?;
        if cancellation.is_cancelled() {
            return Err("Owner message observation cancelled".into());
        }
        if !unread.messages.is_empty() {
            return Ok(());
        }
        let (sender, mut events) = mpsc::channel(1);
        let path = std::fs::canonicalize(self.path()).unwrap_or_else(|_| self.path().to_path_buf());
        let mut watcher = if native {
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
                Ok(event) if !matches!(event.kind, EventKind::Access(_))
                    && (event.need_rescan() || event.paths.iter().any(|changed| changed == &path)) =>
                {
                    let _ = sender.try_send(false);
                }
                Err(_) => { let _ = sender.try_send(true); }
                _ => {}
            }).ok()
        } else { None };
        if let Some(watching) = watcher.as_mut()
            && watching.watch(self.path().parent().ok_or("CHAT.md has no parent")?, RecursiveMode::NonRecursive).is_err()
        {
            watcher = None;
        }
        loop {
            if cancellation.is_cancelled() {
                return Err("Owner message observation cancelled".into());
            }
            // The post-registration check closes the append/watch race. This
            // snapshot also leaves the cursor and agent-wait state untouched.
            let unread = self.unread_snapshot().await?;
            if cancellation.is_cancelled() {
                return Err("Owner message observation cancelled".into());
            }
            if !unread.messages.is_empty() {
                return Ok(());
            }
            let poll = if watcher.is_some() { Duration::from_secs(2) } else { Duration::from_millis(500) };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("Owner message observation cancelled".into()),
                event = events.recv(), if watcher.is_some() => {
                    if event != Some(false) { watcher = None; }
                }
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }

    #[cfg(test)]
    fn begin_agent_wait(&self, remaining: Duration) -> WaitGuard<'_> {
        self.begin_wait(remaining, true)
    }

    fn begin_wait(&self, remaining: Duration, input_required: bool) -> WaitGuard<'_> {
        let mut state = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.input_required = input_required;
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
        self.waiting_until_ms(true)
    }

    pub(crate) fn agent_idle_until_ms(&self) -> Option<u64> {
        self.waiting_until_ms(false)
    }

    fn waiting_until_ms(&self, input_required: bool) -> Option<u64> {
        let state = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .until_ms
            .filter(|_| state.input_required == input_required)
            .filter(|until| *until > super::now_ms())
    }

    pub async fn wait(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<WaitOutcome, String> {
        self.wait_with_watcher(timeout, cancellation, true).await
    }

    pub async fn wait_completed(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<WaitOutcome, String> {
        self.wait_with_policy(timeout, cancellation, true, true)
            .await
    }

    async fn wait_with_watcher(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
        native: bool,
    ) -> Result<WaitOutcome, String> {
        self.wait_with_policy(timeout, cancellation, native, false)
            .await
    }

    async fn wait_with_policy(
        self: &Arc<Self>,
        timeout: Duration,
        cancellation: CancellationToken,
        native: bool,
        completed: bool,
    ) -> Result<WaitOutcome, String> {
        let deadline = Instant::now() + timeout;
        let mut waiting = self.begin_wait(timeout, !completed);
        if cancellation.is_cancelled() {
            return Ok(WaitOutcome::Cancelled);
        }
        if completed {
            self.release_queued_message(super::now_ms()).await?;
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
            if completed {
                self.release_queued_message(super::now_ms()).await?;
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

    #[tokio::test]
    async fn owner_message_wake_observation_preserves_cursor_and_wait_state() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chat/CHAT.md"), true));
        chat.ensure().await.unwrap();
        chat.append_user("pending-owner".into(), "Do this next".into()).await.unwrap();
        let before = chat.unread_snapshot().await.unwrap();
        let _waiting = chat.begin_agent_wait(Duration::from_secs(30));
        let wait_until = chat.agent_waiting_until_ms();
        tokio::time::timeout(Duration::from_secs(1), chat.wait_for_unread(CancellationToken::new())).await.unwrap().unwrap();
        assert_eq!(chat.unread_snapshot().await.unwrap(), before);
        assert_eq!(chat.agent_waiting_until_ms(), wait_until, "observation must not enter or clear an input wait");
    }

    #[tokio::test]
    async fn owner_message_wake_fallback_observes_external_append_and_cancels_without_consuming() {
        let root = tempfile::tempdir().unwrap();
        let chat = Arc::new(ChatFile::new(root.path().join("chat/CHAT.md"), true));
        chat.ensure().await.unwrap();
        let waiting = tokio::spawn({
            let chat = chat.clone();
            async move { chat.wait_for_unread_with_watcher(CancellationToken::new(), false).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::OpenOptions::new().append(true).open(chat.path()).unwrap().write_all(b"Fallback owner message\n").unwrap();
        tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap().unwrap();
        assert!(chat.agent_waiting_until_ms().is_none());
        assert!(chat.read(true).await.unwrap().text.contains("Fallback owner message"));
        let before = chat.unread_snapshot().await.unwrap();
        let cancellation = CancellationToken::new();
        let observing = tokio::spawn({
            let chat = chat.clone(); let cancellation = cancellation.clone();
            async move { chat.wait_for_unread(cancellation).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), observing).await.unwrap().unwrap().is_err());
        assert_eq!(chat.unread_snapshot().await.unwrap(), before);
        assert!(chat.agent_waiting_until_ms().is_none());
    }

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
    fn completed_waits_have_a_separate_bounded_idle_deadline() {
        let root = tempfile::tempdir().unwrap();
        let chat = ChatFile::new(root.path().join("CHAT.md"), false);
        let first = chat.begin_wait(Duration::from_secs(30), false);
        assert!(chat.agent_idle_until_ms().is_some());
        assert!(chat.agent_waiting_until_ms().is_none());
        let question = chat.begin_agent_wait(Duration::from_secs(30));
        drop(first);
        assert!(chat.agent_idle_until_ms().is_none());
        assert!(chat.agent_waiting_until_ms().is_some());
        drop(question);
        let mut timed = chat.begin_wait(Duration::ZERO, false);
        timed.timed_out();
        drop(timed);
        assert!(chat.agent_idle_until_ms().is_some());
        chat.waiting.lock().unwrap().until_ms = Some(super::super::now_ms().saturating_sub(1));
        assert!(chat.agent_idle_until_ms().is_none());
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
        for completed in [false, true] {
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
                        chat.wait_with_policy(
                            Duration::from_secs(30),
                            cancellation,
                            false,
                            completed,
                        )
                        .await
                    }
                });
                tokio::time::timeout(Duration::from_secs(5), async {
                    while chat
                        .agent_waiting_until_ms()
                        .or(chat.agent_idle_until_ms())
                        .is_none()
                    {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .unwrap();
                let summary = chat.owner_summary().await.unwrap();
                assert_eq!(summary.agent_waiting_until_ms.is_some(), !completed);
                assert_eq!(summary.agent_idle_until_ms.is_some(), completed);
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
                    Ok(Ok(WaitOutcome::Message(_))) => {
                        assert!(matches!(outcome, "reply" | "manual"))
                    }
                    Ok(Ok(WaitOutcome::Cancelled)) => assert_eq!(outcome, "cancel"),
                    Err(error) => {
                        assert_eq!(outcome, "drop");
                        assert!(error.is_cancelled());
                    }
                    _ => panic!("unexpected await outcome"),
                }
                assert!(chat.agent_waiting_until_ms().is_none());
                assert!(chat.agent_idle_until_ms().is_none());
                assert!(
                    chat.owner_summary()
                        .await
                        .unwrap()
                        .agent_waiting_until_ms
                        .is_none()
                );
            }
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
