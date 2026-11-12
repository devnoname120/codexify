//! Durable MCP controllers for coding apps without ChatGPT conversation metadata.
use super::{AgentControlRequest, AgentRuntime, SpawnWorkersRequest, WorkerSpec};
use crate::agent_automation::{AgentAutomationStore, CapabilityCatalog};
use crate::exec_sessions::SessionState;
use crate::project_bindings::ConversationIdentity;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

static SETUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_RECORD_BYTES: u64 = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AgentClientRequest {
    Capabilities,
    List {
        after_team_id: Option<String>,
    },
    Setup {
        request_id: String,
        client_name: String,
        project_path: String,
        model: String,
        effort: String,
        #[serde(default)]
        create_worktree: bool,
        #[serde(default)]
        workers: Vec<WorkerSpec>,
    },
    Status {
        team_id: String,
        #[serde(default)]
        include_reports: bool,
    },
    Spawn {
        team_id: String,
        request_id: String,
        workers: Vec<WorkerSpec>,
    },
    Control {
        team_id: String,
        control: AgentControlRequest,
    },
    Inbox {
        team_id: String,
        before: Option<u64>,
        known_revision: Option<String>,
    },
    StopTeam {
        team_id: String,
        request_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientTeam {
    version: u32,
    team_id: String,
    task_key: String,
    request_digest: String,
    client_name: String,
    project_path: String,
    model: String,
    effort: String,
    account_fingerprint: String,
    catalog_revision: String,
    created_at_ms: u64,
    initialized: bool,
}

fn stable_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn identity(team_id: &str) -> Result<ConversationIdentity, String> {
    if team_id.len() != 64
        || !team_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("teamId must be the ID returned by agent_client setup or list".into());
    }
    ConversationIdentity::from_openai_session(&format!("codexify-coding-client:{team_id}"))
        .ok_or_else(|| "Invalid coding-app team identity".into())
}

impl AgentRuntime {
    fn require_client_control(&self) -> Result<(), String> {
        self.require_enabled()?;
        if !self.config.markdown_chat.enabled || !self.config.markdown_chat.automation.enabled {
            return Err(
                "Enable chat and browser automation before coding-app agent control".into(),
            );
        }
        if !self.config.markdown_chat.features.external_agent_control {
            return Err("Enable AI agent control in Settings → Chats & goals first".into());
        }
        Ok(())
    }

    fn client_directory(&self) -> PathBuf {
        crate::memory::task_state_dir(&self.config).join("agent-clients")
    }

    fn read_client_team(&self, team_id: &str) -> Result<ClientTeam, String> {
        let task = identity(team_id)?;
        if std::fs::symlink_metadata(self.client_directory())
            .is_ok_and(|entry| entry.file_type().is_symlink())
        {
            return Err("Coding-app team directory must not be a symlink".into());
        }
        let path = self.client_directory().join(format!("{team_id}.json"));
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| "Unknown teamId; use agent_client list or setup")?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_RECORD_BYTES
        {
            return Err("Invalid coding-app team record".into());
        }
        let record: ClientTeam =
            serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                .map_err(|_| "Invalid coding-app team record")?;
        if record.version != 1 || record.team_id != team_id || record.task_key != task.stable_key()
        {
            return Err("Coding-app team identity does not match its record".into());
        }
        Ok(record)
    }

    fn save_client_team(&self, record: &ClientTeam, new: bool) -> Result<(), String> {
        let directory = self.client_directory();
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        if std::fs::symlink_metadata(&directory)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Coding-app team directory must not be a symlink".into());
        }
        let path = directory.join(format!("{}.json", record.team_id));
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temporary, record).map_err(|e| e.to_string())?;
        temporary.flush().map_err(|e| e.to_string())?;
        if new {
            temporary
                .persist_noclobber(path)
                .map_err(|e| e.to_string())?;
        } else {
            self.read_client_team(&record.team_id)?;
            temporary.persist(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn client_task(&self, team_id: &str) -> Result<ConversationIdentity, String> {
        let record = self.read_client_team(team_id)?;
        if !record.initialized {
            return Err("Team setup is incomplete; retry its original setup request".into());
        }
        let task = identity(team_id)?;
        let root = self
            .bindings
            .selected_project_root(&self.config, &task)?
            .ok_or("Coding-app team workspace is missing; retry setup")?;
        if root != PathBuf::from(&record.project_path) {
            return Err("Team workspace changed; do not control it through an old teamId".into());
        }
        let saved = self.store(&task)?.1.load(&task)?;
        if saved.selection.account_fingerprint.as_deref()
            != Some(record.account_fingerprint.as_str())
        {
            return Err(
                "Coding-app team account changed; create a new team with the current account"
                    .into(),
            );
        }
        if saved.agents.parent_task_key.is_some() {
            return Err("Coding-app controller is no longer a root team".into());
        }
        Ok(task)
    }

    async fn client_capabilities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CapabilityCatalog, String> {
        self.supervisor
            .prepare_new_chat_cancellable(
                self.config
                    .markdown_chat
                    .automation
                    .chatgpt_project_url
                    .as_deref(),
                cancellation,
            )
            .await
    }

    pub async fn client_request(
        &self,
        request: AgentClientRequest,
        cancellation: &CancellationToken,
    ) -> Result<Value, String> {
        self.require_client_control()?;
        if cancellation.is_cancelled() {
            return Err("Coding-app request was cancelled".into());
        }
        match &request {
            AgentClientRequest::Capabilities => {
                let catalog = self.client_capabilities(cancellation).await?;
                Ok(
                    json!({"catalog":catalog,"nextAction":"setup","maxWorkersPerRequest":super::MAX_WORKERS_PER_REQUEST}),
                )
            }
            AgentClientRequest::List { after_team_id } => {
                if let Some(after) = after_team_id {
                    identity(after)?;
                }
                let directory = self.client_directory();
                if !directory.exists() {
                    return Ok(json!({"teams":[],"hasMore":false}));
                }
                let mut ids: Vec<String> = std::fs::read_dir(directory)
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .and_then(|name| name.strip_suffix(".json"))
                            .map(str::to_owned)
                    })
                    .collect();
                ids.sort();
                let mut records: Vec<_> = ids
                    .into_iter()
                    .filter(|id| after_team_id.as_ref().is_none_or(|after| id > after))
                    .filter_map(|id| self.read_client_team(&id).ok())
                    .take(129)
                    .collect();
                let has_more = records.len() > 128;
                records.truncate(128);
                let next = if has_more {
                    records.last().map(|record| record.team_id.clone())
                } else {
                    None
                };
                let teams: Vec<_> = records.into_iter()
                    .map(|record| json!({"teamId":record.team_id,"clientName":record.client_name,
                        "projectPath":record.project_path,"model":record.model,"effort":record.effort,
                        "createdAtMs":record.created_at_ms,"ready":record.initialized})).collect();
                Ok(json!({"teams":teams,"hasMore":has_more,"nextAfterTeamId":next}))
            }
            AgentClientRequest::Setup {
                request_id,
                client_name,
                project_path,
                model,
                effort,
                create_worktree,
                workers,
            } => {
                if !stable_id(request_id)
                    || client_name.trim().is_empty()
                    || client_name.len() > 80
                    || client_name.chars().any(char::is_control)
                    || project_path.trim().is_empty()
                    || project_path.len() > 4096
                    || project_path.contains('\0')
                    || model.is_empty()
                    || model.len() > 256
                    || effort.is_empty()
                    || effort.len() > 256
                {
                    return Err("Setup needs a stable requestId, clientName, projectPath, and model/effort from capabilities".into());
                }
                let spawn = SpawnWorkersRequest {
                    request_id: format!("setup-{:x}", Sha256::digest(request_id.as_bytes())),
                    workers: workers.clone(),
                };
                if !workers.is_empty() {
                    super::validate_request(&spawn)?;
                }
                let _guard = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err("Agent setup was cancelled".into()),
                    guard = SETUP_LOCK.lock() => guard,
                };
                let team_id = format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&(client_name, request_id)).map_err(|e| e.to_string())?
                    )
                );
                let digest = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&request).map_err(|e| e.to_string())?)
                );
                let path = self.client_directory().join(format!("{team_id}.json"));
                let mut record = if std::fs::symlink_metadata(&path).is_ok() {
                    let saved = self.read_client_team(&team_id)?;
                    if saved.request_digest != digest {
                        return Err(
                            "This client/requestId already belongs to different setup arguments"
                                .into(),
                        );
                    }
                    saved
                } else {
                    let catalog = self.client_capabilities(cancellation).await?;
                    let available = catalog.models.iter().any(|candidate| {
                        candidate.id == *model
                            && candidate
                                .efforts
                                .iter()
                                .any(|option| option.id == *effort && option.available)
                    });
                    if !available {
                        return Err("Model/effort is unavailable; call agent_client capabilities before setup".into());
                    }
                    let task = identity(&team_id)?;
                    let record = ClientTeam {
                        version: 1,
                        team_id: team_id.clone(),
                        task_key: task.stable_key().into(),
                        request_digest: digest,
                        client_name: client_name.clone(),
                        project_path: project_path.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                        account_fingerprint: catalog.account_fingerprint,
                        catalog_revision: catalog.catalog_revision,
                        created_at_ms: crate::markdown_chat::now_ms(),
                        initialized: false,
                    };
                    // A durable reservation makes an interrupted setup recoverable.
                    match self.save_client_team(&record, true) {
                        Ok(()) => record,
                        Err(error) => match self.read_client_team(&team_id) {
                            Ok(saved) if saved.request_digest == record.request_digest => saved,
                            _ => return Err(error),
                        },
                    }
                };
                let replayed = record.initialized;
                if !record.initialized {
                    if cancellation.is_cancelled() {
                        return Err("Agent setup was cancelled; retry the same requestId".into());
                    }
                    let task = identity(&team_id)?;
                    let selection = self
                        .bindings
                        .select_project_root(
                            &crate::tools::set_project_root::selection_config(
                                &self.config,
                                Some(*create_worktree),
                            ),
                            &task,
                            project_path,
                        )
                        .await?;
                    let effective = self.bindings.effective_config(&self.config, &task)?;
                    let store = AgentAutomationStore::for_config(&effective)?;
                    let chat = self
                        .chats
                        .chat(&effective, Some(&task), &SessionState::new())?;
                    chat.ensure().await?;
                    chat.set_owner_title(format!("{} · agent coordinator", client_name))
                        .await?;
                    store.set_selection(
                        &task,
                        model,
                        effort,
                        &record.catalog_revision,
                        &record.account_fingerprint,
                        crate::markdown_chat::now_ms(),
                    )?;
                    self.set_delegation(&task, true).await?;
                    record.project_path = selection.project_root.display().to_string();
                    record.initialized = true;
                    self.save_client_team(&record, false)?;
                }
                let task = self.client_task(&team_id)?;
                if cancellation.is_cancelled() {
                    return Err(
                        "Team setup was saved; retry the same setup request to recover its teamId"
                            .into(),
                    );
                }
                let spawn_error = if workers.is_empty() {
                    None
                } else {
                    self.spawn(&task, spawn, false).await.err()
                };
                Ok(
                    json!({"teamId":team_id,"clientName":record.client_name,"projectPath":record.project_path,
                    "ready":true,"replayed":replayed,"spawnError":spawn_error,"team":self.status(&task).await?,
                    "nextAction":"Use status/inbox to inspect progress; control needs the worker taskKey and fresh goal revision. Save teamId to reconnect."}),
                )
            }
            AgentClientRequest::Status { team_id, .. } => {
                Ok(json!({"teamId":team_id,"team":self.status(&self.client_task(team_id)?).await?}))
            }
            AgentClientRequest::Spawn {
                team_id,
                request_id,
                workers,
            } => {
                let task = self.client_task(team_id)?;
                Ok(
                    json!({"teamId":team_id,"team":self.spawn(&task,SpawnWorkersRequest {request_id:request_id.clone(),workers:workers.clone()},false).await?}),
                )
            }
            AgentClientRequest::Control { team_id, control } => Ok(
                json!({"teamId":team_id,"receipt":self.control(&self.client_task(team_id)?,control.clone()).await?}),
            ),
            AgentClientRequest::Inbox {
                team_id,
                before,
                known_revision,
            } => {
                let task = self.client_task(team_id)?;
                let (config, _) = self.store(&task)?;
                let chat = self
                    .chats
                    .chat(&config, Some(&task), &SessionState::new())?;
                let page = chat.widget_page(*before, known_revision.clone()).await?;
                Ok(
                    json!({"teamId":team_id,"revision":page.revision,"messages":page.messages,
                    "hasMore":page.has_more,"before":page.before,"unchanged":page.unchanged}),
                )
            }
            AgentClientRequest::StopTeam {
                team_id,
                request_id,
            } => {
                if !stable_id(request_id) {
                    return Err("stop_team requires a stable requestId".into());
                }
                Ok(
                    json!({"teamId":team_id,"receipt":self.stop_team(&self.client_task(team_id)?,request_id).await?}),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_automation::{
        AgentAutomationSupervisor, AutomationActionState, AutomationExecution,
        AutomationExecutionError, AutomationExecutor, CompanionActionSnapshot, CompanionEvent,
        PROTOCOL_BUILD_SUFFIX,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CancellableExecutor {
        entered: tokio::sync::Notify,
        attempts: AtomicUsize,
        observed: std::sync::Mutex<Option<CancellationToken>>,
        fallback: Executor,
    }
    #[async_trait::async_trait]
    impl AutomationExecutor for CancellableExecutor {
        async fn prepare_new_chat(
            &self,
            project: Option<&str>,
            cancellation: CancellationToken,
        ) -> Result<CapabilityCatalog, String> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                *self.observed.lock().unwrap() = Some(cancellation);
                self.entered.notify_one();
                std::future::pending().await
            } else {
                self.fallback.prepare_new_chat(project, cancellation).await
            }
        }
        async fn execute(
            &self,
            _: AutomationExecution,
            _: tokio::sync::mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            panic!("No browser sends in this test")
        }
    }

    #[tokio::test]
    async fn coding_app_cancelled_browser_discovery_releases_its_lease() {
        let (_root, runtime, _) = fixture();
        let executor = Arc::new(CancellableExecutor {
            entered: tokio::sync::Notify::new(),
            attempts: AtomicUsize::new(0),
            observed: std::sync::Mutex::new(None),
            fallback: Executor {
                preparations: AtomicUsize::new(0),
            },
        });
        let supervisor = AgentAutomationSupervisor::new(
            runtime.config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        let caller = CancellationToken::new();
        let child = caller.clone();
        let runner = supervisor.clone();
        let pending =
            tokio::spawn(async move { runner.prepare_new_chat_cancellable(None, &child).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            executor.entered.notified(),
        )
        .await
        .unwrap();
        caller.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.contains("cancelled"));
        assert!(
            executor
                .observed
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_cancelled()
        );
        let fresh = CancellationToken::new();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                supervisor.prepare_new_chat_cancellable(None, &fresh)
            )
            .await
            .unwrap()
            .is_ok()
        );
        assert!(!fresh.is_cancelled());
        assert!(!runtime.client_directory().exists());
    }

    #[tokio::test]
    async fn coding_app_team_catalog_paginates_without_losing_reconnect_ids() {
        let (_root, runtime, _) = fixture();
        let mut request = setup();
        request["workers"] = json!([]);
        let created = call(&runtime, request).await.unwrap();
        let original = runtime
            .read_client_team(created["teamId"].as_str().unwrap())
            .unwrap();
        for index in 0..130 {
            let mut saved = original.clone();
            saved.team_id = format!("{index:064x}");
            saved.task_key = identity(&saved.team_id).unwrap().stable_key().into();
            saved.initialized = false;
            runtime.save_client_team(&saved, true).unwrap();
        }
        let first = call(&runtime, json!({"action":"list"})).await.unwrap();
        assert_eq!(first["teams"].as_array().unwrap().len(), 128);
        assert_eq!(first["hasMore"], true);
        let second = call(
            &runtime,
            json!({"action":"list","afterTeamId":first["nextAfterTeamId"]}),
        )
        .await
        .unwrap();
        assert_eq!(second["teams"].as_array().unwrap().len(), 3);
        assert_eq!(second["hasMore"], false);
        assert_eq!(second["nextAfterTeamId"], Value::Null);
    }

    struct Executor {
        preparations: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl AutomationExecutor for Executor {
        async fn prepare_new_chat(
            &self,
            _: Option<&str>,
            _: CancellationToken,
        ) -> Result<CapabilityCatalog, String> {
            self.preparations.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::from_value(json!({"protocolVersion":1,"build":format!("test{PROTOCOL_BUILD_SUFFIX}"),
                "accountFingerprint":"test-account","catalogRevision":"test-catalog","models":[{
                "id":"test-model","label":"Test model","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]})).unwrap())
        }
        async fn execute(
            &self,
            _: AutomationExecution,
            _: tokio::sync::mpsc::UnboundedSender<CompanionEvent>,
            _: CancellationToken,
        ) -> Result<CompanionActionSnapshot, AutomationExecutionError> {
            panic!("Test setup queues workers without sending to a real browser")
        }
    }
    fn fixture() -> (tempfile::TempDir, AgentRuntime, Arc<Executor>) {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(access.join("repo")).unwrap();
        let mut config = crate::config::default_config(access);
        config.memory.dir = Some(root.path().join("state").display().to_string());
        config.markdown_chat.features.agent_teams = true;
        config.markdown_chat.features.external_agent_control = true;
        config.markdown_chat.automation.connector_name = "Test connector".into();
        config.markdown_chat.automation.descriptor_path = root
            .path()
            .join("browser/descriptor.json")
            .display()
            .to_string();
        let executor = Arc::new(Executor {
            preparations: AtomicUsize::new(0),
        });
        let supervisor = AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        let runtime = AgentRuntime::new(
            Arc::new(config),
            Arc::default(),
            Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            Arc::new(
                crate::conversation_continuations::ConversationContinuationStore::new(
                    root.path().join("continuations.json"),
                )
                .unwrap(),
            ),
            supervisor,
        );
        (root, runtime, executor)
    }
    async fn call(runtime: &AgentRuntime, args: Value) -> Result<Value, String> {
        runtime
            .client_request(
                serde_json::from_value(args).unwrap(),
                &CancellationToken::new(),
            )
            .await
    }
    fn setup() -> Value {
        json!({"action":"setup","requestId":"initial-team","clientName":"Codex","projectPath":"repo",
        "model":"test-model","effort":"high","workers":[{"role":"Verifier","goal":"Verify the implementation","scope":"tests","readOnly":true}]})
    }

    #[tokio::test]
    async fn coding_app_teams_reconnect_control_report_and_stop_without_chatgpt_metadata() {
        let (root, runtime, executor) = fixture();
        let initial = call(&runtime, setup()).await.unwrap();
        let team = initial["teamId"].as_str().unwrap();
        assert_eq!(initial["ready"], true);
        assert_eq!(initial["spawnError"], Value::Null);
        assert_eq!(runtime.supervisor.queued_count().await, 1);
        let replay = call(&runtime, setup()).await.unwrap();
        assert_eq!(replay["teamId"], team);
        assert_eq!(replay["replayed"], true);
        assert_eq!(executor.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.supervisor.queued_count().await, 1);
        let mut changed = setup();
        changed["effort"] = json!("other");
        assert!(call(&runtime, changed).await.is_err());

        // A fresh runtime and project-binding store recover the same team.
        let reconnected = AgentRuntime::new(
            runtime.config.clone(),
            runtime.chats.clone(),
            Arc::new(crate::project_bindings::ProjectBindingStore::new(
                root.path().join("bindings"),
            )),
            runtime.continuations.clone(),
            runtime.supervisor.clone(),
        );
        let listed = call(&reconnected, json!({"action":"list"})).await.unwrap();
        assert_eq!(listed["teams"][0]["teamId"], team);
        let status = call(&reconnected, json!({"action":"status","teamId":team}))
            .await
            .unwrap();
        let key = status["team"]["workers"][0]["taskKey"].as_str().unwrap();
        let worker =
            crate::project_bindings::ConversationIdentity::from_stable_keys(key.into(), key.into())
                .unwrap();
        let (_, store) = runtime.store(&worker).unwrap();
        // Simulate completed browser pairing; no real prompt is sent by this test.
        runtime.supervisor.discard_queued_actions(&worker).await;
        store
            .mutate(&worker, |saved| {
                saved.pairing.creating_chat = false;
                saved.pairing.pending = None;
                saved.pairing.candidate_url = None;
                saved.pairing.canonical_url = Some("https://chatgpt.com/c/test-worker".into());
                saved.pairing.verified_at_ms = Some(1);
                saved.pairing.verified_owner_generation = Some(0);
                saved.action = AutomationActionState::default();
                Ok(())
            })
            .unwrap();
        let revision = store.load(&worker).unwrap().goal.unwrap().revision;
        let pause = json!({"action":"control","teamId":team,"control":{"requestId":"pause-once","workerTaskKey":key,
            "action":"pause","expectedRevision":revision}});
        assert!(call(&runtime, pause.clone()).await.is_ok());
        assert!(call(&runtime, pause).await.is_ok());
        let latest = store.load(&worker).unwrap().goal.unwrap().revision;
        assert!(call(&runtime,json!({"action":"control","teamId":team,"control":{"requestId":"resume-once", "workerTaskKey":key,
            "action":"resume","expectedRevision":latest}})).await.is_ok());
        assert!(call(&runtime,json!({"action":"control","teamId":team,"control":{"requestId":"instruction-once", "workerTaskKey":key,
            "action":"message","content":"Verify the edge cases and report the command output"}})).await.is_ok());
        let report = runtime
            .report(&worker, "Verified edge cases with passing commands")
            .await
            .unwrap();
        assert_eq!(report["parentNotified"], true);
        let inbox = call(&runtime, json!({"action":"inbox","teamId":team}))
            .await
            .unwrap();
        assert!(inbox["messages"].as_array().unwrap().iter().any(|message| {
            message["markdown"]
                .as_str()
                .unwrap()
                .contains("Verified edge cases")
        }));
        let unchanged = call(
            &runtime,
            json!({"action":"inbox","teamId":team,"knownRevision":inbox["revision"]}),
        )
        .await
        .unwrap();
        assert_eq!(unchanged["unchanged"], true);
        assert!(!root.path().join("projects/repo/agents.txt").exists());
        let stopped = call(
            &runtime,
            json!({"action":"stop_team","teamId":team,"requestId":"stop-all"}),
        )
        .await
        .unwrap();
        assert_eq!(stopped["receipt"]["stopped"], true);
        let replay = call(&runtime, setup()).await.unwrap();
        assert!(replay["spawnError"].as_str().unwrap().contains("Resume"));
        assert!(
            runtime
                .chats
                .chat(
                    &runtime.store(&worker).unwrap().0,
                    Some(&worker),
                    &SessionState::new()
                )
                .unwrap()
                .agent_stop_requested()
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn coding_app_controls_fail_closed_for_disabled_unknown_foreign_and_changed_teams() {
        let (_root, mut runtime, _) = fixture();
        Arc::make_mut(&mut runtime.config)
            .markdown_chat
            .features
            .external_agent_control = false;
        assert!(
            call(&runtime, setup())
                .await
                .unwrap_err()
                .contains("AI agent control")
        );
        assert!(!runtime.client_directory().exists());
        Arc::make_mut(&mut runtime.config)
            .markdown_chat
            .features
            .external_agent_control = true;
        assert!(
            call(&runtime, json!({"action":"status","teamId":"a".repeat(64)}))
                .await
                .is_err()
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            runtime
                .client_request(serde_json::from_value(setup()).unwrap(), &cancellation)
                .await
                .is_err()
        );
        let first = call(&runtime, setup()).await.unwrap();
        let mut another = setup();
        another["clientName"] = json!("Other app");
        another["requestId"] = json!("other-team");
        let second = call(&runtime, another).await.unwrap();
        let key = first["team"]["workers"][0]["taskKey"].clone();
        assert!(call(&runtime,json!({"action":"control","teamId":second["teamId"],"control":{"requestId":"foreign-stop",
            "workerTaskKey":key,"action":"stop","expectedRevision":1}})).await.unwrap_err().contains("owned"));
        let task = identity(first["teamId"].as_str().unwrap()).unwrap();
        runtime
            .store(&task)
            .unwrap()
            .1
            .mutate(&task, |saved| {
                saved.agents.parent_task_key = Some("b".repeat(64));
                Ok(())
            })
            .unwrap();
        assert!(
            call(
                &runtime,
                json!({"action":"status","teamId":first["teamId"]})
            )
            .await
            .is_err()
        );
    }
}
