use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use crate::types::AppConfig;

pub(crate) const OUTPUT_FIELD: &str = "new_codexify_ticket";
pub(crate) const OUTPUT_DESCRIPTION: &str =
    "Pass this as codexify_ticket on the next tool call, even after an error.";
pub(crate) const INSTRUCTIONS: &str = concat!(
    "## Agent tickets\n\nAgent tickets are enabled. Omit codexify_ticket until the first response supplies new_codexify_ticket. ",
    "Then pass the latest new_codexify_ticket as codexify_ticket on every Codexify model-facing tool call, including setup, chat, polling, and MCP discovery tools. ",
    "Call Codexify tools serially and wait for each result; accepted connector error responses also return a new_codexify_ticket. ",
    "A transport failure supplies no replacement: keep the previous ticket, and do not blindly repeat an operation that may already have run. ",
    "A ticket rejection means another branch may have claimed or advanced the chain, or a response was lost. ",
    "Inform the user without another tool call and stop this agent branch, even if other instructions require chat_write or chat_await. ",
    "Do not retry a rejection, guess, repeat setup, or recover tickets from logs, files, or another branch. ",
    "After five minutes without a completed ticketed call and with no call in flight, the next call can reclaim the chain with no ticket or a stale ticket. ",
    "Do not wait for expiry or retry on your own. Only resume after the user asks, or recover by starting a new conversation or disabling experimental.agentTickets."
);
pub(crate) const REJECTED: &str = "Ticket rejected; this call did not run. Another or duplicated agent may have claimed or advanced this conversation, or a response was lost. Stop this agent branch now, including chat_write/chat_await. Do not retry, guess, repeat setup, or retrieve tickets from logs or state. Inform the user of this warning without another tool call. Only the user may recover.";
pub(crate) const WARNING: &str = concat!(
    "ChatGPT started a duplicated agent on this same project. ",
    "This is a ChatGPT bug and it\u{2019}s problematic because then two agents can fight to do edits and overwrite each other. ",
    "The duplicated agent was asked to stop in order to let the other agent work without interference"
);

pub(crate) struct Ticket {
    value: String,
    last_activity: SystemTime,
}

type TicketCell = Arc<AsyncMutex<Option<Ticket>>>;

#[derive(Default)]
pub(crate) struct AgentTicketStore {
    directory: Option<PathBuf>,
    conversations: Mutex<HashMap<ConversationIdentity, TicketCell>>,
}

enum TicketState {
    File(File),
    Memory(OwnedMutexGuard<Option<Ticket>>),
}

pub(crate) struct TicketPermit {
    state: TicketState,
    next: String,
    acceptance: &'static str,
}

#[derive(Debug)]
pub(crate) struct TicketFailure {
    reason: &'static str,
    message: String,
}

impl TicketFailure {
    fn rejected(reason: &'static str) -> Self {
        Self {
            reason,
            message: REJECTED.into(),
        }
    }

    fn state(error: impl std::fmt::Display) -> Self {
        Self {
            reason: "state_unavailable",
            message: state_error(error),
        }
    }

    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }
}

impl std::fmt::Display for TicketFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl TicketPermit {
    pub(crate) fn acceptance(&self) -> &'static str {
        self.acceptance
    }

    pub(crate) async fn commit(
        self,
        cancellation: CancellationToken,
    ) -> Result<Option<String>, String> {
        tokio::task::spawn_blocking(move || self.commit_sync(&cancellation))
            .await
            .map_err(|error| {
                format!("Ticket handoff failed after dispatch; work may have run: {error}")
            })?
    }

    fn commit_sync(self, cancellation: &CancellationToken) -> Result<Option<String>, String> {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        match self.state {
            TicketState::File(mut file) => {
                // Replacing this inode would let concurrent handles lock different state copies.
                let mut save = || -> std::io::Result<()> {
                    file.rewind()?;
                    file.write_all(self.next.as_bytes())?;
                    file.sync_all()
                };
                save().map_err(|error| format!("Ticket handoff failed after dispatch; work may have run. Do not retry blindly: {error}"))?;
            }
            TicketState::Memory(mut current) => {
                *current = Some(Ticket {
                    value: self.next.clone(),
                    last_activity: SystemTime::now(),
                })
            }
        }
        Ok(Some(self.next))
    }
}

impl AgentTicketStore {
    pub(crate) fn persistent(directory: PathBuf) -> Self {
        Self {
            directory: Some(directory),
            ..Self::default()
        }
    }

    pub(crate) fn for_current_user(config: &AppConfig) -> Result<Self, String> {
        if !config.experimental.agent_tickets {
            return Ok(Self::default());
        }
        let home =
            crate::util::home_dir().ok_or("experimental.agentTickets requires a home directory")?;
        let scope = Sha256::digest(format!("{}\0{}", config.work_dir.display(), config.port));
        Ok(Self::persistent(
            home.join(".codexify/agent-tickets")
                .join(format!("{scope:x}")),
        ))
    }

    pub(crate) async fn reserve(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
        supplied: Option<String>,
    ) -> Result<TicketPermit, TicketFailure> {
        let cell = if let Some(identity) = conversation {
            if let Some(directory) = &self.directory {
                let path = directory.join(format!("{}.ticket", identity.stable_key()));
                return tokio::task::spawn_blocking(move || {
                    reserve_file(&path, supplied.as_deref())
                })
                .await
                .map_err(TicketFailure::state)?;
            }
            let mut conversations = self
                .conversations
                .lock()
                .map_err(|_| TicketFailure::state("poisoned state"))?;
            conversations.entry(identity.clone()).or_default().clone()
        } else {
            session.agent_ticket.clone()
        };
        let current = cell
            .try_lock_owned()
            .map_err(|_| TicketFailure::rejected("in_flight"))?;
        let offline = current
            .as_ref()
            .is_some_and(|ticket| is_offline(ticket.last_activity));
        let (next, acceptance) = successor(
            current.as_ref().map(|ticket| ticket.value.as_str()),
            supplied.as_deref(),
            offline,
        )?;
        Ok(TicketPermit {
            state: TicketState::Memory(current),
            next,
            acceptance,
        })
    }

    #[cfg(test)]
    async fn advance(
        &self,
        conversation: Option<&ConversationIdentity>,
        session: &SessionState,
        supplied: Option<String>,
    ) -> Result<String, String> {
        self.reserve(conversation, session, supplied)
            .await
            .map_err(|error| error.to_string())?
            .commit(CancellationToken::new())
            .await
            .map(Option::unwrap)
    }
}

fn state_error(error: impl std::fmt::Display) -> String {
    format!("Ticket state unavailable; this call did not run. Ask the user to resolve it: {error}")
}

fn valid_ticket(ticket: &str) -> bool {
    ticket.len() == 8
        && ticket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn is_offline(last_activity: SystemTime) -> bool {
    last_activity.elapsed().is_ok_and(|elapsed| {
        elapsed >= Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS)
    })
}

fn successor(
    current: Option<&str>,
    supplied: Option<&str>,
    offline: bool,
) -> Result<(String, &'static str), TicketFailure> {
    let acceptance = match (current, supplied) {
        (None, None) => "initial",
        (Some(current), Some(supplied)) if current == supplied => "matched",
        (Some(_), None) if offline => "reclaimed_missing",
        (Some(_), Some(_)) if offline => "reclaimed_stale",
        (Some(_), None) => return Err(TicketFailure::rejected("missing")),
        _ => return Err(TicketFailure::rejected("mismatch")),
    };
    loop {
        let mut bytes = [0u8; 6];
        getrandom::getrandom(&mut bytes).map_err(TicketFailure::state)?;
        let next = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        if Some(next.as_str()) != current {
            return Ok((next, acceptance));
        }
    }
}

fn reserve_file(path: &Path, supplied: Option<&str>) -> Result<TicketPermit, TicketFailure> {
    std::fs::create_dir_all(path.parent().expect("ticket path has a parent"))
        .map_err(TicketFailure::state)?;
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
    let offline = !current.is_empty()
        && is_offline(
            file.metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(TicketFailure::state)?,
        );
    let (next, acceptance) = successor(
        (!current.is_empty()).then_some(current.as_str()),
        supplied,
        offline,
    )?;
    Ok(TicketPermit {
        state: TicketState::File(file),
        next,
        acceptance,
    })
}

pub(crate) fn take_ticket(args: &mut Value) -> Result<Option<String>, String> {
    match args
        .as_object_mut()
        .and_then(|args| args.remove("codexify_ticket"))
    {
        None => Ok(None),
        Some(Value::String(ticket)) => Ok(Some(ticket)),
        _ => Err(REJECTED.into()),
    }
}

fn needs_arguments_envelope(schema: &Value) -> bool {
    schema["additionalProperties"] != false
        || schema["properties"].get("codexify_ticket").is_some()
        || contains_local_reference(schema)
        || schema.as_object().is_some_and(|object| {
            object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "$schema"
                        | "$id"
                        | "$defs"
                        | "definitions"
                        | "$comment"
                        | "title"
                        | "description"
                        | "type"
                        | "properties"
                        | "required"
                        | "additionalProperties"
                )
            })
        })
}

fn contains_local_reference(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef")
                && value
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#')))
                || contains_local_reference(value)
        }),
        Value::Array(values) => values.iter().any(contains_local_reference),
        _ => false,
    }
}

pub(crate) fn input_schema(mut original: Value) -> Value {
    let ticket = json!({
        "type":"string",
        "description":"Latest new_codexify_ticket; omit only for the first agent call."
    });
    if needs_arguments_envelope(&original) {
        // A separate schema resource preserves root-local references inside upstream arguments.
        let id = if original["$schema"]
            .as_str()
            .is_some_and(|schema| schema.contains("draft-04"))
        {
            "id"
        } else {
            "$id"
        };
        original
            .as_object_mut()
            .unwrap()
            .entry(id)
            .or_insert(json!("urn:codexify:ticket-arguments"));
        json!({
            "type":"object",
            "properties":{"codexify_ticket":ticket, "arguments":original},
            "required":["arguments"],
            "additionalProperties":false
        })
    } else {
        original
            .as_object_mut()
            .unwrap()
            .entry("properties")
            .or_insert(json!({}))["codexify_ticket"] = ticket;
        original
    }
}

pub(crate) fn unwrap_arguments(args: &mut Value, schema: &Value) -> Result<(), String> {
    if !needs_arguments_envelope(schema) {
        return Ok(());
    }
    if args.as_object().is_none_or(|args| args.len() != 1) || !args["arguments"].is_object() {
        return Err(
            "Pass original tool arguments inside the advertised `arguments` object.".into(),
        );
    }
    *args = args["arguments"].take();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_envelopes_preserve_recursive_and_draft_four_references() {
        let recursive = json!({
            "type":"object", "properties":{"child":{"$ref":"#"}}, "additionalProperties":false
        });
        let schema = input_schema(recursive);
        assert!(jsonschema::is_valid(
            &schema,
            &json!({"codexify_ticket":"12345678", "arguments":{"child":{}}})
        ));
        assert!(!jsonschema::is_valid(
            &schema,
            &json!({"codexify_ticket":"12345678", "arguments":{"child":{"codexify_ticket":"12345678"}}})
        ));
        let draft_four = json!({
            "$schema":"http://json-schema.org/draft-04/schema#",
            "type":"object", "properties":{"value":{"$ref":"#/definitions/number"}},
            "definitions":{"number":{"type":"integer"}},
            "required":["value"], "additionalProperties":false
        });
        let schema = input_schema(draft_four);
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!({"arguments":{"value":42}})));
        assert!(!validator.is_valid(&json!({"arguments":{"value":"not an integer"}})));
    }

    fn advance_file(path: &Path, supplied: Option<&str>) -> Result<String, String> {
        reserve_file(path, supplied)
            .map_err(|error| error.to_string())?
            .commit_sync(&CancellationToken::new())
            .map(Option::unwrap)
    }

    #[test]
    fn experimental_agent_tickets_recover_after_offline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.ticket");
        let ticket = advance_file(&path, None).unwrap();
        let offline = std::time::SystemTime::now()
            - std::time::Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(offline)
            .unwrap();
        let recovered =
            advance_file(&path, None).expect("an offline conversation must be reclaimable");
        assert_ne!(recovered, ticket);
        assert!(advance_file(&path, None).is_err());
        assert!(advance_file(&path, Some(&ticket)).is_err());
        assert!(advance_file(&path, Some(&recovered)).is_ok());
    }

    #[test]
    fn offline_ticket_recovery_never_steals_an_in_flight_call() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.ticket");
        let ticket = advance_file(&path, None).unwrap();
        let offline =
            SystemTime::now() - Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(offline)
            .unwrap();
        let permit = reserve_file(&path, Some("wrong ticket shape")).unwrap();
        assert_eq!(permit.acceptance(), "reclaimed_stale");
        assert!(reserve_file(&path, None).is_err());
        assert!(reserve_file(&path, Some("oldstate")).is_err());
        let current = permit
            .commit_sync(&CancellationToken::new())
            .unwrap()
            .unwrap();
        assert!(advance_file(&path, None).is_err());
        assert!(advance_file(&path, Some(&current)).is_ok());
        assert_ne!(ticket, current);
    }

    #[test]
    fn rejected_tickets_do_not_extend_the_offline_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.ticket");
        advance_file(&path, None).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(advance_file(&path, Some("oldstate")).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let future = SystemTime::now() + Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        assert!(advance_file(&path, None).is_err());
    }

    #[tokio::test]
    async fn memory_tickets_recover_once_after_offline() {
        let store = AgentTicketStore::default();
        let session = SessionState::new();
        let old = store.advance(None, &session, None).await.unwrap();
        session
            .agent_ticket
            .lock()
            .await
            .as_mut()
            .unwrap()
            .last_activity =
            SystemTime::now() - Duration::from_millis(crate::markdown_chat::OFFLINE_AFTER_MS + 1);
        let permit = store.reserve(None, &session, None).await.unwrap();
        assert!(store.reserve(None, &session, None).await.is_err());
        let new = permit
            .commit(CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(old, new);
        assert!(store.advance(None, &session, Some(old)).await.is_err());
        assert!(store.advance(None, &session, Some(new)).await.is_ok());
    }

    #[tokio::test]
    async fn interrupted_reservations_preserve_tickets_and_exclude_concurrent_calls() {
        let root = tempfile::tempdir().unwrap();
        let identity = ConversationIdentity::from_openai_session("interrupted").unwrap();
        for (store, conversation) in [
            (AgentTicketStore::default(), None),
            (AgentTicketStore::default(), Some(&identity)),
            (
                AgentTicketStore::persistent(root.path().to_path_buf()),
                Some(&identity),
            ),
        ] {
            let session = SessionState::new();
            let ticket = store.advance(conversation, &session, None).await.unwrap();
            let permit = store
                .reserve(conversation, &session, Some(ticket.clone()))
                .await
                .unwrap();
            assert!(
                store
                    .reserve(conversation, &session, Some(ticket.clone()))
                    .await
                    .is_err()
            );
            drop(permit);
            let permit = store
                .reserve(conversation, &session, Some(ticket.clone()))
                .await
                .unwrap();
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            assert!(permit.commit(cancellation).await.unwrap().is_none());
            assert!(
                store
                    .advance(conversation, &session, Some(ticket))
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn conversation_tickets_span_transports_without_crossing_conversations() {
        let store = AgentTicketStore::default();
        let a = SessionState::new();
        let b = SessionState::new();
        let identity = ConversationIdentity::from_openai_session("one").unwrap();
        let other = ConversationIdentity::from_openai_session("two").unwrap();
        let ticket = store.advance(Some(&identity), &a, None).await.unwrap();
        assert!(store.advance(Some(&identity), &b, None).await.is_err());
        let next = store
            .advance(Some(&identity), &b, Some(ticket.clone()))
            .await
            .unwrap();
        assert_ne!(ticket, next);
        assert!(
            store
                .advance(Some(&identity), &a, Some(ticket))
                .await
                .is_err()
        );
        assert!(store.advance(Some(&other), &a, None).await.is_ok());
        assert!(store.advance(Some(&identity), &a, Some(next)).await.is_ok());
    }

    #[tokio::test]
    async fn anonymous_tickets_are_transport_local() {
        let store = AgentTicketStore::default();
        let a = SessionState::new();
        let b = SessionState::new();
        let ticket = store.advance(None, &a, None).await.unwrap();
        assert!(store.advance(None, &a, None).await.is_err());
        assert!(store.advance(None, &b, Some(ticket.clone())).await.is_err());
        assert!(store.advance(None, &b, None).await.is_ok());
        assert!(store.advance(None, &a, Some(ticket)).await.is_ok());
    }

    #[test]
    fn persistent_tickets_have_one_winner_and_survive_reopening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.ticket");
        let ticket = advance_file(&path, None).unwrap();
        let barrier = std::sync::Barrier::new(16);
        let results = std::thread::scope(|scope| {
            (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        advance_file(&path, Some(&ticket))
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winners = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), *winners[0]);
        assert!(advance_file(&path, None).is_err());
        assert!(advance_file(&path, Some(&ticket)).is_err());
        assert!(advance_file(&path, Some(winners[0])).is_ok());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn reloaded_store_does_not_reissue_a_lost_response() {
        let root = tempfile::tempdir().unwrap();
        let identity = ConversationIdentity::from_openai_session("persistent").unwrap();
        let session = SessionState::new();
        let store = AgentTicketStore::persistent(root.path().to_path_buf());
        let ticket = store
            .advance(Some(&identity), &session, None)
            .await
            .unwrap();
        let next = store
            .advance(Some(&identity), &session, Some(ticket.clone()))
            .await
            .unwrap();
        drop(store);
        let reloaded = AgentTicketStore::persistent(root.path().to_path_buf());
        assert!(
            reloaded
                .advance(Some(&identity), &session, None)
                .await
                .is_err()
        );
        let error = reloaded
            .advance(Some(&identity), &session, Some(ticket))
            .await
            .unwrap_err();
        assert!(!error.contains(&next));
        assert!(
            reloaded
                .advance(Some(&identity), &session, Some(next))
                .await
                .is_ok()
        );
    }

    #[test]
    fn corrupt_or_unwritable_state_never_bootstraps_again() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("conversation.ticket");
        for corrupt in ["short", "123456789", "!!!!!!!!", "abc\ndefg"] {
            std::fs::write(&path, corrupt).unwrap();
            assert!(advance_file(&path, None).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
        assert!(advance_file(&path.join("not-a-directory"), None).is_err());
    }

    #[test]
    fn non_string_tickets_are_rejected_without_disclosing_a_successor() {
        for ticket in [Value::Null, json!(42)] {
            assert_eq!(
                take_ticket(&mut json!({"codexify_ticket":ticket})).unwrap_err(),
                REJECTED
            );
        }
        assert!(take_ticket(&mut json!({})).unwrap().is_none());
        for ticket in ["ab_CD-12", "", "too long to be a ticket", "abc defg"] {
            assert_eq!(
                take_ticket(&mut json!({"codexify_ticket":ticket})).unwrap(),
                Some(ticket.into())
            );
        }
    }

    #[test]
    fn ticket_schema_preserves_colliding_upstream_arguments_and_local_refs() {
        for original in [
            json!({"type":"object", "additionalProperties":true}),
            json!({
                "type":"object", "properties":{"codexify_ticket":{"$ref":"#/$defs/number"}},
                "$defs":{"number":{"type":"integer"}},
                "required":["codexify_ticket"], "additionalProperties":false
            }),
            json!({
                "type":"object", "properties":{"codexify_ticket":{"type":"integer"}},
                "required":["codexify_ticket"], "maxProperties":1, "additionalProperties":false
            }),
        ] {
            let schema = input_schema(original.clone());
            let mut args =
                json!({"codexify_ticket":"12345678", "arguments":{"codexify_ticket":42}});
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(validator.is_valid(&args), "{schema}");
            assert_eq!(take_ticket(&mut args).unwrap().as_deref(), Some("12345678"));
            unwrap_arguments(&mut args, &original).unwrap();
            assert_eq!(args, json!({"codexify_ticket":42}));
            assert!(jsonschema::is_valid(&original, &args));
            assert!(unwrap_arguments(&mut json!({"arguments":{},"extra":1}), &original).is_err());
        }
    }

    #[test]
    fn closed_native_schema_stays_flat_and_keeps_required_arguments() {
        let original = json!({
            "type":"object", "properties":{"value":{"type":"integer"}},
            "required":["value"], "additionalProperties":false
        });
        let schema = input_schema(original.clone());
        assert!(jsonschema::is_valid(&schema, &json!({"value":1})));
        assert!(jsonschema::is_valid(
            &schema,
            &json!({"value":1,"codexify_ticket":"12345678"})
        ));
        assert!(!jsonschema::is_valid(
            &schema,
            &json!({"codexify_ticket":"12345678"})
        ));
        assert!(!jsonschema::is_valid(
            &schema,
            &json!({"value":1,"codexify_ticket":null})
        ));
        let mut args = json!({"value":1,"codexify_ticket":"12345678"});
        take_ticket(&mut args).unwrap();
        unwrap_arguments(&mut args, &original).unwrap();
        assert_eq!(args, json!({"value":1}));
    }
}
