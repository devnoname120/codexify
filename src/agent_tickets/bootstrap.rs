//! Host-issued, single-use admission for a new browser prompt. It does not
//! invalidate the running branch until the new branch reserves its first call.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct BrowserTicketScope {
    pub task_key: String,
    pub owner_generation: u64,
    pub conversation_key: Option<String>,
}

pub(crate) struct TicketCall<'a> {
    pub conversation: Option<&'a ConversationIdentity>,
    pub task: Option<&'a ConversationIdentity>,
    pub owner_generation: Option<u64>,
    pub tool: &'a str,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct BrowserGrant {
    scope: BrowserTicketScope,
    expires_at_ms: u64,
}

impl BrowserGrant {
    fn check(&self, call: &TicketCall<'_>) -> Result<bool, TicketFailure> {
        if crate::markdown_chat::now_ms() > self.expires_at_ms {
            return Err(TicketFailure::rejected("browser_ticket_expired"));
        }
        let physical = call
            .conversation
            .ok_or_else(|| TicketFailure::rejected("browser_ticket_scope"))?;
        if let Some(expected) = &self.scope.conversation_key {
            if physical.stable_key() != expected
                || !call
                    .task
                    .is_some_and(|task| task.stable_key() == self.scope.task_key)
                || call.owner_generation != Some(self.scope.owner_generation)
            {
                return Err(TicketFailure::rejected("browser_ticket_scope"));
            }
            return Ok(false);
        }
        if call
            .task
            .is_some_and(|task| task.stable_key() == self.scope.task_key)
            && call.owner_generation == Some(self.scope.owner_generation)
        {
            return Ok(false);
        }
        // setup precedes continue_task in a newly created ChatGPT conversation.
        // It may bootstrap only an empty chain; setup still checks its own ref.
        if call.tool == crate::conversation_auth::AUTHORIZATION_TOOL_WIRE_NAME
            && call.owner_generation == Some(0)
            && call.task == call.conversation
        {
            return Ok(true);
        }
        Err(TicketFailure::rejected("browser_ticket_scope"))
    }
}

impl AgentTicketStore {
    pub(crate) fn issue_browser_ticket(&self, scope: BrowserTicketScope) -> Result<String, String> {
        let grant = BrowserGrant {
            scope,
            expires_at_ms: crate::markdown_chat::now_ms().saturating_add(60 * 60 * 1000),
        };
        loop {
            let (ticket, _) = successor(None, None, false).map_err(|error| error.to_string())?;
            if let Some(root) = &self.directory {
                let directory = root.join("browser-prompts");
                std::fs::create_dir_all(&directory).map_err(state_error)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                        .map_err(state_error)?;
                }
                // Bound storage by expiry without exposing grant contents.
                for entry in std::fs::read_dir(&directory)
                    .map_err(state_error)?
                    .take(1024)
                    .flatten()
                {
                    if entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(7200))
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
                if directory.join(format!("{ticket}.used")).exists() {
                    continue;
                }
                let path = directory.join(format!("{ticket}.json"));
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = match options.open(&path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(state_error(error)),
                };
                let written = serde_json::to_writer(&mut file, &grant)
                    .map_err(std::io::Error::other)
                    .and_then(|_| file.sync_all());
                if let Err(error) = written {
                    let _ = std::fs::remove_file(path);
                    return Err(state_error(error));
                }
                return Ok(ticket);
            }
            let mut pending = self
                .browser_tickets
                .lock()
                .map_err(|_| state_error("poisoned browser ticket state"))?;
            pending.retain(|_, grant| grant.expires_at_ms >= crate::markdown_chat::now_ms());
            self.browser_tickets_used
                .lock()
                .map_err(|_| state_error("poisoned browser ticket state"))?
                .retain(|_, expiry| *expiry >= crate::markdown_chat::now_ms());
            if self
                .browser_tickets_used
                .lock()
                .map_err(|_| state_error("poisoned browser ticket state"))?
                .contains_key(&ticket)
            {
                continue;
            }
            if pending.contains_key(&ticket) {
                continue;
            }
            pending.insert(ticket.clone(), grant);
            return Ok(ticket);
        }
    }

    fn claim_browser_ticket(
        &self,
        ticket: &str,
        call: &TicketCall<'_>,
    ) -> Result<Option<bool>, TicketFailure> {
        if !valid_ticket(ticket) {
            return Ok(None);
        }
        if let Some(root) = &self.directory {
            let directory = root.join("browser-prompts");
            let path = directory.join(format!("{ticket}.json"));
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if directory.join(format!("{ticket}.used")).exists() {
                        return Err(TicketFailure::rejected("browser_ticket_used"));
                    }
                    return Ok(None);
                }
                Err(error) => return Err(TicketFailure::state(error)),
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 2048 {
                return Err(TicketFailure::state("invalid browser ticket state"));
            }
            let grant: BrowserGrant = serde_json::from_reader(
                File::open(&path).map_err(TicketFailure::state)?.take(2048),
            )
            .map_err(TicketFailure::state)?;
            let empty_only = grant.check(call)?;
            match std::fs::rename(&path, directory.join(format!("{ticket}.used"))) {
                Ok(()) => return Ok(Some(empty_only)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(TicketFailure::rejected("browser_ticket_used"));
                }
                Err(error) => return Err(TicketFailure::state(error)),
            }
        }
        let mut pending = self
            .browser_tickets
            .lock()
            .map_err(|_| TicketFailure::state("poisoned browser ticket state"))?;
        let Some(grant) = pending.get(ticket) else {
            if self
                .browser_tickets_used
                .lock()
                .map_err(|_| TicketFailure::state("poisoned browser ticket state"))?
                .contains_key(ticket)
            {
                return Err(TicketFailure::rejected("browser_ticket_used"));
            }
            return Ok(None);
        };
        let empty_only = grant.check(call)?;
        self.browser_tickets_used
            .lock()
            .map_err(|_| TicketFailure::state("poisoned browser ticket state"))?
            .insert(ticket.into(), grant.expires_at_ms);
        pending.remove(ticket);
        Ok(Some(empty_only))
    }

    pub(crate) async fn reserve_for_call(
        &self,
        call: TicketCall<'_>,
        session: &SessionState,
        supplied: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<TicketPermit, TicketFailure> {
        let bootstrap = match supplied.as_deref() {
            Some(ticket) => self.claim_browser_ticket(ticket, &call)?,
            None => None,
        };
        let Some(empty_only) = bootstrap else {
            return self.reserve(call.conversation, session, supplied).await;
        };
        let physical = call
            .conversation
            .ok_or_else(|| TicketFailure::rejected("browser_ticket_scope"))?;
        // Only host-issued admission waits for a preceding branch's call to
        // finish/cancel. Ordinary overlapping and stale tickets still reject.
        loop {
            if cancellation.is_cancelled() {
                return Err(TicketFailure::state(
                    "browser ticket admission was cancelled; no call ran",
                ));
            }
            let reserved = if let Some(root) = &self.directory {
                let path = root.join(format!("{}.ticket", physical.stable_key()));
                tokio::task::spawn_blocking(move || reserve_bootstrap_file(&path, empty_only))
                    .await
                    .map_err(TicketFailure::state)?
            } else {
                let cell = self
                    .conversations
                    .lock()
                    .map_err(|_| TicketFailure::state("poisoned ticket state"))?
                    .entry(physical.clone())
                    .or_default()
                    .clone();
                match cell.try_lock_owned() {
                    Ok(current) => {
                        if empty_only && current.is_some() {
                            return Err(TicketFailure::rejected("browser_ticket_scope"));
                        }
                        let next = browser_successor(
                            current.as_ref().map(|value| value.value.as_str()),
                            supplied.as_deref(),
                        )?;
                        Ok(TicketPermit {
                            state: TicketState::Memory(current),
                            next,
                            acceptance: "browser_prompt",
                        })
                    }
                    Err(_) => Err(TicketFailure::rejected("in_flight")),
                }
            };
            match reserved {
                Ok(mut permit) => {
                    // Never return the consumed one-time grant as its successor.
                    while Some(permit.next.as_str()) == supplied.as_deref() {
                        permit.next = successor(None, None, false)?.0;
                    }
                    return Ok(permit);
                }
                Err(error) if error.reason() == "in_flight" => tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {},
                    _ = cancellation.cancelled() => {},
                },
                Err(error) => return Err(error),
            }
        }
    }
}

fn browser_successor(
    current: Option<&str>,
    supplied: Option<&str>,
) -> Result<String, TicketFailure> {
    loop {
        let next = successor(current, current, false)?.0;
        if Some(next.as_str()) != supplied {
            return Ok(next);
        }
    }
}

fn reserve_bootstrap_file(path: &Path, empty_only: bool) -> Result<TicketPermit, TicketFailure> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(TicketFailure::state)?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => TicketFailure::rejected("in_flight"),
        error => TicketFailure::state(error),
    })?;
    let mut current = String::new();
    Read::by_ref(&mut file)
        .take(9)
        .read_to_string(&mut current)
        .map_err(TicketFailure::state)?;
    if !current.is_empty() && !valid_ticket(&current) {
        return Err(TicketFailure::state("invalid stored ticket"));
    }
    if empty_only && !current.is_empty() {
        return Err(TicketFailure::rejected("browser_ticket_scope"));
    }
    let current = (!current.is_empty()).then_some(current.as_str());
    Ok(TicketPermit {
        state: TicketState::File(file),
        next: browser_successor(current, None)?,
        acceptance: "browser_prompt",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call<'a>(
        physical: &'a ConversationIdentity,
        task: &'a ConversationIdentity,
        generation: u64,
        tool: &'a str,
    ) -> TicketCall<'a> {
        TicketCall {
            conversation: Some(physical),
            task: Some(task),
            owner_generation: Some(generation),
            tool,
        }
    }
    fn scope(physical: &ConversationIdentity) -> BrowserTicketScope {
        BrowserTicketScope {
            task_key: physical.stable_key().into(),
            owner_generation: 0,
            conversation_key: Some(physical.stable_key().into()),
        }
    }

    #[tokio::test]
    async fn browser_tickets_admit_one_new_branch_and_preserve_the_chain_until_redeemed() {
        for persistent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = if persistent {
                AgentTicketStore::persistent(root.path().into())
            } else {
                AgentTicketStore::default()
            };
            let session = SessionState::new();
            let identity = ConversationIdentity::from_openai_session("browser-ticket").unwrap();
            let old = store
                .advance(Some(&identity), &session, None)
                .await
                .unwrap();
            let bootstrap = store.issue_browser_ticket(scope(&identity)).unwrap();
            assert!(valid_ticket(&bootstrap));
            // Issuing a queued prompt never revokes the currently running agent.
            let old = store
                .advance(Some(&identity), &session, Some(old))
                .await
                .unwrap();
            let permit = store
                .reserve_for_call(
                    call(&identity, &identity, 0, "chat_read"),
                    &session,
                    Some(bootstrap.clone()),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(permit.acceptance(), "browser_prompt");
            assert!(
                store
                    .reserve(Some(&identity), &session, Some(old.clone()))
                    .await
                    .is_err()
            );
            assert!(
                store
                    .reserve_for_call(
                        call(&identity, &identity, 0, "chat_read"),
                        &session,
                        Some(bootstrap.clone()),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
            let next = permit
                .commit(CancellationToken::new())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(next, bootstrap);
            assert!(
                store
                    .reserve(Some(&identity), &session, Some(old))
                    .await
                    .is_err()
            );
            assert!(
                store
                    .reserve_for_call(
                        call(&identity, &identity, 0, "chat_read"),
                        &session,
                        Some(bootstrap),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
            store
                .advance(Some(&identity), &session, Some(next))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn browser_ticket_waits_for_in_flight_work_without_weakening_ordinary_ticket_checks() {
        for persistent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let store = Arc::new(if persistent {
                AgentTicketStore::persistent(root.path().into())
            } else {
                AgentTicketStore::default()
            });
            let session = Arc::new(SessionState::new());
            let identity =
                ConversationIdentity::from_openai_session("busy-browser-ticket").unwrap();
            let old = store
                .advance(Some(&identity), &session, None)
                .await
                .unwrap();
            let in_flight = store
                .reserve(Some(&identity), &session, Some(old.clone()))
                .await
                .unwrap();
            let bootstrap = store.issue_browser_ticket(scope(&identity)).unwrap();
            let waiting = tokio::spawn({
                let store = store.clone();
                let session = session.clone();
                let identity = identity.clone();
                async move {
                    store
                        .reserve_for_call(
                            call(&identity, &identity, 0, "chat_read"),
                            &session,
                            Some(bootstrap),
                            CancellationToken::new(),
                        )
                        .await
                }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!waiting.is_finished());
            assert!(
                store
                    .reserve(Some(&identity), &session, Some(old))
                    .await
                    .is_err()
            );
            in_flight.commit(CancellationToken::new()).await.unwrap();
            let permit = tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            permit.commit(CancellationToken::new()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn browser_ticket_scope_and_restart_do_not_allow_replay_or_an_unrelated_chat() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionState::new();
        let identity = ConversationIdentity::from_openai_session("scope-browser-ticket").unwrap();
        let other = ConversationIdentity::from_openai_session("other-browser-ticket").unwrap();
        let bootstrap = AgentTicketStore::persistent(root.path().into())
            .issue_browser_ticket(scope(&identity))
            .unwrap();
        let store = AgentTicketStore::persistent(root.path().into());
        for wrong in [
            call(&other, &other, 0, "chat_read"),
            call(&identity, &identity, 1, "chat_read"),
        ] {
            assert!(
                store
                    .reserve_for_call(
                        wrong,
                        &session,
                        Some(bootstrap.clone()),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
        }
        store
            .reserve_for_call(
                call(&identity, &identity, 0, "chat_read"),
                &session,
                Some(bootstrap.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .commit(CancellationToken::new())
            .await
            .unwrap();
        let restarted = AgentTicketStore::persistent(root.path().into());
        assert!(
            restarted
                .reserve_for_call(
                    call(&identity, &identity, 0, "chat_read"),
                    &session,
                    Some(bootstrap),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn new_conversation_ticket_bootstraps_setup_or_the_claimed_task_but_not_arbitrary_work() {
        let store = AgentTicketStore::default();
        let session = SessionState::new();
        let physical = ConversationIdentity::from_openai_session("new-physical").unwrap();
        let task = ConversationIdentity::from_openai_session("continued-task").unwrap();
        let scope = BrowserTicketScope {
            task_key: task.stable_key().into(),
            owner_generation: 1,
            conversation_key: None,
        };
        let ticket = store.issue_browser_ticket(scope.clone()).unwrap();
        assert!(
            store
                .reserve_for_call(
                    call(&physical, &physical, 0, "exec_command"),
                    &session,
                    Some(ticket.clone()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        let next = store
            .reserve_for_call(
                call(
                    &physical,
                    &physical,
                    0,
                    crate::conversation_auth::AUTHORIZATION_TOOL_WIRE_NAME,
                ),
                &session,
                Some(ticket),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .commit(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        store
            .advance(Some(&physical), &session, Some(next))
            .await
            .unwrap();
        let ticket = store.issue_browser_ticket(scope).unwrap();
        store
            .reserve_for_call(
                call(&physical, &task, 1, "automation_verify_pairing"),
                &session,
                Some(ticket),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .commit(CancellationToken::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_browser_admission_does_not_steal_an_in_flight_ticket() {
        let store = Arc::new(AgentTicketStore::default());
        let session = Arc::new(SessionState::new());
        let identity =
            ConversationIdentity::from_openai_session("cancelled-browser-admission").unwrap();
        let current = store
            .advance(Some(&identity), &session, None)
            .await
            .unwrap();
        let active = store
            .reserve(Some(&identity), &session, Some(current))
            .await
            .unwrap();
        let ticket = store.issue_browser_ticket(scope(&identity)).unwrap();
        let cancellation = CancellationToken::new();
        let waiting = tokio::spawn({
            let store = store.clone();
            let session = session.clone();
            let identity = identity.clone();
            let cancellation = cancellation.clone();
            async move {
                store
                    .reserve_for_call(
                        call(&identity, &identity, 0, "chat_read"),
                        &session,
                        Some(ticket),
                        cancellation,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let next = active
            .commit(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        store
            .advance(Some(&identity), &session, Some(next))
            .await
            .unwrap();
    }
}
