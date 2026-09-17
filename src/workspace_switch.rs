use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChange {
    pub revision: String,
    pub previous_root: PathBuf,
    pub awaiting_selection: bool,
}

impl WorkspaceChange {
    pub fn notice(&self, active: Option<&Path>) -> String {
        match active {
            Some(path) => format!(
                "The user switched this conversation's workspace. Previous path: {}. Current path: {}. Stop using the previous workspace assumptions. Before further project work, call get_agent_brief to load the new environment, AGENTS.md, skills and saved state, then recall as needed. Existing files and running commands in the previous workspace were not moved or deleted.",
                self.previous_root.display(),
                path.display()
            ),
            None => format!(
                "The user opened workspace selection. The previous workspace ({}) is no longer selected. Do not select scratch or guess a project. Leave the setup picker open; use chat_await to wait for the user's selection. Existing files were preserved.",
                self.previous_root.display()
            ),
        }
    }

    pub(crate) fn new(previous_root: PathBuf) -> Self {
        Self {
            revision: format!(
                "{}-{}",
                chrono::Utc::now().timestamp_micros(),
                ATOMIC_COUNTER.fetch_add(1, Ordering::SeqCst)
            ),
            previous_root,
            awaiting_selection: true,
        }
    }
}

impl ProjectBindingStore {
    pub async fn dispatch_workspace(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<(Option<PathBuf>, Option<WorkspaceChange>), String> {
        if !config.multi_project {
            return Ok((Some(config.work_dir.clone()), None));
        }
        let root = canonical_access_root(config)?;
        let _lock = acquire_lock(&self.binding_path(&root, identity)).await?;
        Ok((
            self.selected_project_root(config, identity)?,
            self.pending_change_at(&root, identity)?,
        ))
    }

    pub async fn reuse_worktree(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
        project: &str,
        path: &Path,
    ) -> Result<ProjectRootSelection, String> {
        let (root, source) = resolve_project_root(config, project)?;
        let binding_path = self.binding_path(&root, identity);
        let _lock = acquire_lock(&binding_path).await?;
        self.clear_suspended_selection(&root, identity)?;
        let choice = crate::worktrees::existing_worktree_choice(config, &source, path).await?;
        if let Some(existing) = self.selected_project_root(config, identity)? {
            if existing == choice.path
                && let ProjectBindingState::Project(selection) =
                    self.binding_state(config, identity)?
            {
                return Ok(selection);
            }
            return Err("Use Switch to another project in the setup card before choosing a different worktree.".into());
        }
        let metadata = if choice.managed_worktree {
            Some(load_metadata(
                &metadata_path_for_worktree(&choice.git_root)
                    .ok_or("Missing managed-worktree metadata")?,
            )?)
        } else {
            None
        };
        let stored = StoredProjectBinding {
            version: BINDING_VERSION,
            access_root: root.to_string_lossy().into_owned(),
            source_project_root: Some(source.to_string_lossy().into_owned()),
            project_root: choice.path.to_string_lossy().into_owned(),
            repository_url: None,
            managed_worktree: choice.managed_worktree,
            worktree_git_root: (!choice.source_checkout)
                .then(|| choice.git_root.to_string_lossy().into_owned()),
            worktrees_root: metadata.map(|meta| meta.worktrees_root),
        };
        let parent = binding_path.parent().ok_or("Missing binding directory")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut temp, &stored).map_err(|error| error.to_string())?;
        temp.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        let resolved = self
            .read_binding(temp.path(), &root)?
            .ok_or("Worktree binding disappeared")?;
        temp.persist(&binding_path)
            .map_err(|error| error.to_string())?;
        self.finish_workspace_selection(&root, identity)?;
        let _ = crate::worktrees::touch_workspace(config, &choice.path);
        Ok(selection_from_binding(root, resolved, config.worktrees.mode, true, false,
            ProjectBindingScope::ChatGptConversation,
            vec!["Reusing the selected worktree unchanged. Other conversations may still use it; avoid concurrent edits.".into()]))
    }

    fn change_path(&self, access_root: &Path, identity: &ConversationIdentity) -> PathBuf {
        self.binding_path(access_root, identity)
            .with_extension("workspace-change")
    }

    pub(super) fn pending_change_at(
        &self,
        access_root: &Path,
        identity: &ConversationIdentity,
    ) -> Result<Option<WorkspaceChange>, String> {
        let path = self.change_path(access_root, identity);
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() <= 16_384 => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("Invalid workspace-change record: {error}")),
            Ok(_) => Err("Workspace-change record is oversized".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("Cannot read workspace-change record: {error}")),
        }
    }

    pub fn pending_workspace_change(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
    ) -> Result<Option<WorkspaceChange>, String> {
        if !config.multi_project {
            return Ok(None);
        }
        self.pending_change_at(&canonical_access_root(config)?, identity)
    }

    fn save_change(
        &self,
        access_root: &Path,
        identity: &ConversationIdentity,
        change: &WorkspaceChange,
    ) -> Result<(), String> {
        let path = self.change_path(access_root, identity);
        let parent = path.parent().ok_or("Missing workspace-change directory")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut temp, change).map_err(|error| error.to_string())?;
        temp.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        temp.persist(path).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn active_record_paths(&self, root: &Path, identity: &ConversationIdentity) -> [PathBuf; 3] {
        [
            self.binding_path(root, identity),
            self.without_project_path(root, identity),
            self.legacy_binding_path(root, identity),
        ]
    }

    pub(super) fn clear_suspended_selection(
        &self,
        root: &Path,
        identity: &ConversationIdentity,
    ) -> Result<(), String> {
        if !self
            .pending_change_at(root, identity)?
            .is_some_and(|change| change.awaiting_selection)
        {
            return Ok(());
        }
        for path in self.active_record_paths(root, identity) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("Cannot clear previous workspace binding: {error}"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish_workspace_selection(
        &self,
        root: &Path,
        identity: &ConversationIdentity,
    ) -> Result<(), String> {
        if let Some(mut change) = self.pending_change_at(root, identity)? {
            change.awaiting_selection = false;
            self.save_change(root, identity, &change)?;
        }
        Ok(())
    }

    pub async fn switch_to_picker(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
        expected_path: &Path,
    ) -> Result<(), String> {
        if !config.multi_project {
            return Err("Workspace switching requires multi-project mode".into());
        }
        let root = canonical_access_root(config)?;
        let _lock = acquire_lock(&self.binding_path(&root, identity)).await?;
        if let Some(change) = self.pending_change_at(&root, identity)?
            && change.awaiting_selection
            && change.previous_root == expected_path
        {
            return self.clear_suspended_selection(&root, identity);
        }
        let current = self
            .selected_project_root(config, identity)?
            .ok_or("No workspace is selected")?;
        if current != expected_path {
            return Err(
                "Workspace changed since this card was displayed. Refresh it before switching."
                    .into(),
            );
        }
        let change = WorkspaceChange::new(current);
        let archive = self
            .access_root_dir(&root)
            .join("previous-workspaces")
            .join(&change.revision);
        std::fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
        for path in self.active_record_paths(&root, identity) {
            if path.is_file() {
                std::fs::copy(
                    &path,
                    archive.join(path.file_name().ok_or("Invalid binding filename")?),
                )
                .map_err(|error| error.to_string())?;
            }
        }
        // Suspending first prevents a partially completed reset from dispatching into the old root.
        self.save_change(&root, identity, &change)?;
        self.clear_suspended_selection(&root, identity)
    }

    pub async fn acknowledge_workspace_change(
        &self,
        config: &AppConfig,
        identity: &ConversationIdentity,
        revision: &str,
        active_root: &Path,
    ) -> Result<(), String> {
        let root = canonical_access_root(config)?;
        let _lock = acquire_lock(&self.binding_path(&root, identity)).await?;
        if self
            .pending_change_at(&root, identity)?
            .is_some_and(|change| !change.awaiting_selection && change.revision == revision)
            && self.selected_project_root(config, identity)?.as_deref() == Some(active_root)
        {
            std::fs::remove_file(self.change_path(&root, identity))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn previous_binding_files(&self, root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(self.access_root_dir(root).join("previous-workspaces"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .flat_map(|entry| {
                std::fs::read_dir(entry.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
            })
            .collect()
    }

    pub(super) fn saved_binding_files(&self, root: &Path) -> Vec<PathBuf> {
        self.binding_files(root)
            .into_iter()
            .chain(
                self.previous_binding_files(root)
                    .into_iter()
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json")),
            )
            .collect()
    }
}
