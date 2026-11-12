//! Ticket recovery waits share ordinary admission and its in-flight lock. A
//! pending waiter never commits, so it cannot refresh the active branch's lease.
use super::*;

pub(crate) enum TicketWaitOutcome {
    Renewed(TicketPermit),
    Pending,
    Interrupted,
}

impl AgentTicketStore {
    pub(crate) async fn wait_for_ticket(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> Result<TicketWaitOutcome, TicketFailure> {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            if cancellation.is_cancelled() {
                return Ok(TicketWaitOutcome::Interrupted);
            }
            let failure = match self.reserve(conversation, session, None).await {
                Ok(permit) => return Ok(TicketWaitOutcome::Renewed(permit)),
                Err(error) if matches!(error.reason(), "in_flight" | "missing") => None,
                Err(error) => Some(error),
            };
            if tokio::time::Instant::now() >= deadline {
                return failure.map_or(Ok(TicketWaitOutcome::Pending), Err);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(TicketWaitOutcome::Interrupted),
                _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_millis(250))) => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn expire(store: &AgentTicketStore, identity: &ConversationIdentity) {
        let old =
            SystemTime::now() - Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
        if let Some(root) = &store.directory {
            File::options()
                .write(true)
                .open(root.join(format!("{}.ticket", identity.stable_key())))
                .unwrap()
                .set_modified(old)
                .unwrap();
        } else {
            let cell = store
                .conversations
                .lock()
                .unwrap()
                .get(identity)
                .unwrap()
                .clone();
            cell.lock().await.as_mut().unwrap().last_activity = old;
        }
    }

    #[tokio::test]
    async fn pending_ticket_waits_do_not_extend_or_change_the_active_ticket() {
        for persistent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = if persistent {
                AgentTicketStore::persistent(root.path().into())
            } else {
                AgentTicketStore::default()
            };
            let session = SessionState::new();
            let identity = ConversationIdentity::from_openai_session("wait-active").unwrap();
            let ticket = store
                .advance(Some(&identity), &session, None)
                .await
                .unwrap();
            let before = if persistent {
                std::fs::metadata(
                    root.path()
                        .join(format!("{}.ticket", identity.stable_key())),
                )
                .unwrap()
                .modified()
                .unwrap()
            } else {
                let cell = store
                    .conversations
                    .lock()
                    .unwrap()
                    .get(&identity)
                    .unwrap()
                    .clone();
                cell.lock().await.as_ref().unwrap().last_activity
            };
            for _ in 0..2 {
                assert!(matches!(
                    store
                        .wait_for_ticket(
                            Some(&identity),
                            &session,
                            Duration::from_millis(15),
                            CancellationToken::new()
                        )
                        .await
                        .unwrap(),
                    TicketWaitOutcome::Pending
                ));
            }
            let permit = store
                .reserve(Some(&identity), &session, Some(ticket))
                .await
                .unwrap();
            match &permit.state {
                TicketState::File(file) => {
                    assert_eq!(file.metadata().unwrap().modified().unwrap(), before)
                }
                TicketState::Memory(current) => {
                    assert_eq!(current.as_ref().unwrap().last_activity, before)
                }
            }
        }
    }

    #[tokio::test]
    async fn ticket_wait_renews_only_after_inactivity_with_one_winner_and_no_cross_chat_reclaim() {
        for persistent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = if persistent {
                AgentTicketStore::persistent(root.path().into())
            } else {
                AgentTicketStore::default()
            };
            let session = SessionState::new();
            let identity = ConversationIdentity::from_openai_session("wait-renew").unwrap();
            let other = ConversationIdentity::from_openai_session("wait-other").unwrap();
            let old = store
                .advance(Some(&identity), &session, None)
                .await
                .unwrap();
            let other_ticket = store.advance(Some(&other), &session, None).await.unwrap();
            expire(&store, &identity).await;
            let (a, b) = tokio::join!(
                store.wait_for_ticket(
                    Some(&identity),
                    &session,
                    Duration::from_millis(30),
                    CancellationToken::new()
                ),
                store.wait_for_ticket(
                    Some(&identity),
                    &session,
                    Duration::from_millis(30),
                    CancellationToken::new()
                ),
            );
            let permit = match (a.unwrap(), b.unwrap()) {
                (TicketWaitOutcome::Renewed(permit), TicketWaitOutcome::Pending)
                | (TicketWaitOutcome::Pending, TicketWaitOutcome::Renewed(permit)) => permit,
                _ => panic!("only one waiter may acquire admission"),
            };
            let renewed = permit
                .commit(CancellationToken::new())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(renewed, old);
            assert!(
                store
                    .reserve(Some(&identity), &session, Some(old))
                    .await
                    .is_err()
            );
            assert!(matches!(
                store
                    .wait_for_ticket(
                        Some(&identity),
                        &session,
                        Duration::ZERO,
                        CancellationToken::new()
                    )
                    .await
                    .unwrap(),
                TicketWaitOutcome::Pending
            ));
            store
                .advance(Some(&other), &session, Some(other_ticket))
                .await
                .unwrap();
            if persistent {
                let restarted = AgentTicketStore::persistent(root.path().into());
                restarted
                    .advance(Some(&identity), &session, Some(renewed))
                    .await
                    .unwrap();
            } else {
                store
                    .advance(Some(&identity), &session, Some(renewed))
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn expired_tickets_still_wait_for_in_flight_calls_and_cancellation_preserves_the_chain() {
        for persistent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = if persistent {
                AgentTicketStore::persistent(root.path().into())
            } else {
                AgentTicketStore::default()
            };
            let session = SessionState::new();
            let identity = ConversationIdentity::from_openai_session("wait-inflight").unwrap();
            let ticket = store
                .advance(Some(&identity), &session, None)
                .await
                .unwrap();
            expire(&store, &identity).await;
            let permit = store
                .reserve(Some(&identity), &session, Some(ticket))
                .await
                .unwrap();
            assert!(matches!(
                store
                    .wait_for_ticket(
                        Some(&identity),
                        &session,
                        Duration::from_millis(15),
                        CancellationToken::new()
                    )
                    .await
                    .unwrap(),
                TicketWaitOutcome::Pending
            ));
            let cancel = CancellationToken::new();
            let interrupt = async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                cancel.cancel();
            };
            let (outcome, ()) = tokio::join!(
                store.wait_for_ticket(
                    Some(&identity),
                    &session,
                    Duration::from_secs(10),
                    cancel.clone()
                ),
                interrupt
            );
            assert!(matches!(outcome.unwrap(), TicketWaitOutcome::Interrupted));
            let next = permit
                .commit(CancellationToken::new())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                store
                    .wait_for_ticket(
                        Some(&identity),
                        &session,
                        Duration::ZERO,
                        CancellationToken::new()
                    )
                    .await
                    .unwrap(),
                TicketWaitOutcome::Pending
            ));
            store
                .advance(Some(&identity), &session, Some(next))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn ticket_wait_supports_transport_sessions_without_conversation_metadata() {
        let store = AgentTicketStore::default();
        let session = SessionState::new();
        let ticket = store.advance(None, &session, None).await.unwrap();
        session
            .agent_ticket
            .lock()
            .await
            .as_mut()
            .unwrap()
            .last_activity =
            SystemTime::now() - Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
        let TicketWaitOutcome::Renewed(permit) = store
            .wait_for_ticket(None, &session, Duration::ZERO, CancellationToken::new())
            .await
            .unwrap()
        else {
            panic!("expired transport chain must renew")
        };
        let next = permit
            .commit(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(next, ticket);
        store.advance(None, &session, Some(next)).await.unwrap();
    }
}
