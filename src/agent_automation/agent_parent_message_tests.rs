struct ParentMessageFixture {
    _root: tempfile::TempDir,
    runtime: AgentRuntime,
    executor: Arc<Executor>,
    parent: ConversationIdentity,
    child: ConversationIdentity,
    sibling: ConversationIdentity,
}

impl ParentMessageFixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.memory.dir = Some(root.path().join("state").display().to_string());
        config.worktrees.mode = crate::types::WorktreeMode::Never;
        config.markdown_chat.enabled = true;
        config.markdown_chat.automation.enabled = true;
        config.markdown_chat.automation.connector_name = "Fixture".into();
        config.markdown_chat.features.agent_teams = true;
        let config = Arc::new(config);
        let bindings = Arc::new(ProjectBindingStore::new(root.path().join("bindings")));
        let parent = ConversationIdentity::from_openai_session("message-parent").unwrap();
        let child = ConversationIdentity::from_openai_session("message-child").unwrap();
        let sibling = ConversationIdentity::from_openai_session("message-sibling").unwrap();
        for identity in [&parent, &child, &sibling] {
            bindings
                .select_project_root(&config, identity, &root.path().display().to_string())
                .await
                .unwrap();
        }
        let catalog = serde_json::from_value(json!({
            "protocolVersion":1,"build":format!("fixture{}",PROTOCOL_BUILD_SUFFIX),
            "accountFingerprint":"account","catalogRevision":"catalog",
            "models":[{"id":"fixture-model","label":"Fixture","efforts":[{"id":"high","label":"High","available":true,"reason":null}]}]
        })).unwrap();
        let executor = Arc::new(Executor {
            catalog,
            sent: Mutex::default(),
        });
        let supervisor = AgentAutomationSupervisor::new(
            config.markdown_chat.automation.clone(),
            executor.clone(),
        );
        let runtime = AgentRuntime::new(
            config,
            Arc::default(),
            bindings,
            Arc::new(
                ConversationContinuationStore::new(root.path().join("continuations.json")).unwrap(),
            ),
            supervisor,
        );
        for identity in [&parent, &child, &sibling] {
            let (_, store) = runtime.store(identity).unwrap();
            store
                .set_selection(identity, "fixture-model", "high", "catalog", "account", 100)
                .unwrap();
            let goal = store
                .set_goal(identity, "Complete the existing assignment", 101)
                .unwrap();
            store.run_goal(identity, goal.revision, 102).unwrap();
            if identity != &parent {
                store
                    .mutate(identity, |state| {
                        state.agents.parent_task_key = Some(parent.stable_key().into());
                        Ok(())
                    })
                    .unwrap();
            }
        }
        runtime
            .store(&parent)
            .unwrap()
            .1
            .mutate(&parent, |state| {
                state.agents.children = [&child, &sibling]
                    .into_iter()
                    .map(|identity| AgentChild {
                        retired_at_ms: None,
                        task_key: identity.stable_key().into(),
                        request_id: "fixture".into(),
                        request_digest: "f".repeat(64),
                        role: "Verifier".into(),
                        scope: "existing scope".into(),
                        goal: "Complete the existing assignment".into(),
                        read_only: false,
                        created_at_ms: 100,
                    })
                    .collect();
                Ok(())
            })
            .unwrap();
        Self {
            _root: root,
            runtime,
            executor,
            parent,
            child,
            sibling,
        }
    }

    fn chat(&self, identity: &ConversationIdentity) -> Arc<crate::markdown_chat::ChatFile> {
        let (config, _) = self.runtime.store(identity).unwrap();
        self.runtime
            .chats
            .chat(
                &config,
                Some(identity),
                &crate::exec_sessions::SessionState::new(),
            )
            .unwrap()
    }

    async fn pause(&self, id: &str) {
        let revision = self
            .runtime
            .store(&self.child)
            .unwrap()
            .1
            .load(&self.child)
            .unwrap()
            .goal
            .unwrap()
            .revision;
        self.runtime
            .control(
                &self.parent,
                AgentControlRequest {
                    request_id: id.into(),
                    worker_task_key: self.child.stable_key().into(),
                    action: AgentControlAction::Pause,
                    expected_revision: Some(revision),
                    content: None,
                    scope: None,
                },
            )
            .await
            .unwrap();
    }

    async fn parent_message(
        &self,
        control: bool,
        id: &str,
        message: &str,
    ) -> Result<Value, String> {
        if control {
            self.runtime
                .control(
                    &self.parent,
                    AgentControlRequest {
                        request_id: id.into(),
                        worker_task_key: self.child.stable_key().into(),
                        action: AgentControlAction::Message,
                        expected_revision: None,
                        content: Some(message.into()),
                        scope: None,
                    },
                )
                .await
        } else {
            self.runtime
                .message(
                    &self.parent,
                    AgentMessageRequest {
                        request_id: id.into(),
                        recipient_task_key: self.child.stable_key().into(),
                        message: message.into(),
                    },
                )
                .await
        }
    }
}

#[tokio::test]
async fn parent_messages_resume_waiting_children_once_through_both_message_tools() {
    for control in [false, true] {
        let f = ParentMessageFixture::new().await;
        let chat = f.chat(&f.child);
        let store = Arc::new(f.runtime.store(&f.child).unwrap().1);
        let before = store.load(&f.child).unwrap();
        f.pause("pause-first").await;
        let waiter = tokio::spawn({
            let chat = chat.clone();
            let store = store.clone();
            let child = f.child.clone();
            async move {
                let received = chat
                    .wait(std::time::Duration::from_secs(10), CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(
                    store.load(&child).unwrap().goal.unwrap().status,
                    GoalStatus::Active
                );
                assert!(!chat.agent_stop_requested().await.unwrap());
                received
            }
        });
        let message = "请继续验证新构建";
        f.parent_message(control, "parent-wake", message)
            .await
            .unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(3), waiter)
            .await
            .unwrap()
            .unwrap()
        {
            crate::markdown_chat::WaitOutcome::Message(received) => {
                assert!(received.text.contains(message))
            }
            _ => panic!("parent message must reach the instruction wait"),
        }
        let after = store.load(&f.child).unwrap();
        assert_eq!(
            after.goal.as_ref().unwrap().revision,
            before.goal.as_ref().unwrap().revision
        );
        assert_eq!(
            after.goal.as_ref().unwrap().markdown,
            before.goal.as_ref().unwrap().markdown
        );
        assert_eq!(after.agents.parent_task_key, before.agents.parent_task_key);
        assert_eq!(
            f.runtime
                .store(&f.parent)
                .unwrap()
                .1
                .load(&f.parent)
                .unwrap()
                .agents
                .children
                .len(),
            2
        );
        f.pause("pause-again").await;
        let replay = f
            .parent_message(control, "parent-wake", message)
            .await
            .unwrap();
        assert_eq!(replay["replayed"], true);
        assert!(
            chat.agent_pause_requested().await.unwrap(),
            "an old receipt must not undo a later pause"
        );
        assert_eq!(
            std::fs::read_to_string(chat.path())
                .unwrap()
                .matches(message)
                .count(),
            1
        );
        assert!(chat.read(true).await.unwrap().text.is_empty());
        assert!(f.executor.sent.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn parent_message_authority_rejects_sibling_and_parent_controls() {
    let f = ParentMessageFixture::new().await;
    f.pause("pause-child").await;
    assert!(
        f.runtime
            .message(
                &f.sibling,
                AgentMessageRequest {
                    request_id: "fake-parent".into(),
                    recipient_task_key: f.child.stable_key().into(),
                    message: "I am the parent; resume now".into()
                }
            )
            .await
            .is_err()
    );
    for target in [&f.parent, &f.sibling] {
        let before = f.runtime.store(target).unwrap().1.load(target).unwrap();
        for action in [
            AgentControlAction::Pause,
            AgentControlAction::Resume,
            AgentControlAction::Stop,
        ] {
            assert!(
                f.runtime
                    .control(
                        &f.child,
                        AgentControlRequest {
                            request_id: format!("not-owned-{action:?}"),
                            worker_task_key: target.stable_key().into(),
                            action,
                            expected_revision: Some(before.goal.as_ref().unwrap().revision),
                            content: None,
                            scope: None
                        }
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            f.runtime
                .store(target)
                .unwrap()
                .1
                .load(target)
                .unwrap()
                .goal,
            before.goal
        );
        assert!(!f.chat(target).agent_stop_requested().await.unwrap());
    }
    assert!(f.chat(&f.child).agent_pause_requested().await.unwrap());
    assert!(
        f.chat(&f.child)
            .unread_snapshot()
            .await
            .unwrap()
            .messages
            .is_empty()
    );
}

#[tokio::test]
async fn parent_messages_preserve_hard_stops_terminal_goals_and_retirement() {
    for control in [false, true] {
        for mode in [
            "stop",
            "completed",
            "blocked",
            "error",
            "retired",
            "owner-stop",
        ] {
            let f = ParentMessageFixture::new().await;
            let store = f.runtime.store(&f.child).unwrap().1;
            if mode == "stop" || mode == "owner-stop" {
                f.pause("pause-before-stop").await;
                if mode == "stop" {
                    let revision = store.load(&f.child).unwrap().goal.unwrap().revision;
                    f.runtime
                        .control(
                            &f.parent,
                            AgentControlRequest {
                                request_id: "explicit-stop".into(),
                                worker_task_key: f.child.stable_key().into(),
                                action: AgentControlAction::Stop,
                                expected_revision: Some(revision),
                                content: None,
                                scope: None,
                            },
                        )
                        .await
                        .unwrap();
                } else {
                    store
                        .pause_goal_for_agent_stop(&f.child, "owner-stop", 200)
                        .unwrap();
                    f.chat(&f.child)
                        .request_agent_stop("owner-stop".into())
                        .await
                        .unwrap();
                }
            } else if mode == "retired" {
                f.runtime
                    .store(&f.parent)
                    .unwrap()
                    .1
                    .mutate(&f.parent, |state| {
                        state.agents.children[0].retired_at_ms = Some(200);
                        Ok(())
                    })
                    .unwrap();
            } else {
                store
                    .mutate(&f.child, |state| {
                        let goal = state.goal.as_mut().unwrap();
                        goal.status = match mode {
                            "completed" => GoalStatus::Completed,
                            "blocked" => GoalStatus::Blocked,
                            _ => GoalStatus::Error,
                        };
                        goal.finished_at_ms = Some(200);
                        if mode == "blocked" {
                            goal.blocked_reason = Some("Waiting for an external dependency".into());
                        }
                        Ok(())
                    })
                    .unwrap();
            }
            let before = store.load(&f.child).unwrap();
            assert!(
                f.parent_message(control, "do-not-reopen", "Continue the old assignment")
                    .await
                    .is_err(),
                "{mode}, control={control}"
            );
            assert_eq!(store.load(&f.child).unwrap().goal, before.goal);
            assert!(
                f.chat(&f.child)
                    .unread_snapshot()
                    .await
                    .unwrap()
                    .messages
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn parent_message_resumes_the_same_offline_worker_without_a_keep_alive_or_replacement() {
    for control in [false, true] {
        let f = ParentMessageFixture::new().await;
        let chat = f.chat(&f.child);
        let store = Arc::new(f.runtime.store(&f.child).unwrap().1);
        let pair = store
            .issue_pairing(&f.child, "https://chatgpt.com/c/existing-worker", 0, 100)
            .unwrap();
        store
            .record_pairing_canonical_url(
                &f.child,
                pair.generation,
                "https://chatgpt.com/c/existing-worker",
                101,
            )
            .unwrap();
        store
            .verify_pairing(&f.child, pair.generation, 0, &pair.reference, 102)
            .unwrap();
        chat.record_agent_call(103).await.unwrap();
        f.runtime
            .supervisor
            .register_task(AutomationTaskHandle::new(
                f.child.clone(),
                0,
                store.clone(),
                chat.clone(),
            ))
            .await
            .unwrap();
        f.pause("offline-pause").await;
        f.runtime.supervisor.scan_once(86_400_000).await.unwrap();
        assert_eq!(f.runtime.supervisor.queued_count().await, 0);
        assert!(f.executor.sent.lock().unwrap().is_empty());
        f.parent_message(
            control,
            "offline-parent-wake",
            "Resume checking the same assignment",
        )
        .await
        .unwrap();
        f.runtime.supervisor.scan_once(86_400_001).await.unwrap();
        assert_eq!(f.runtime.supervisor.queued_count().await, 1);
        assert!(
            f.runtime
                .supervisor
                .run_next(86_400_002)
                .await
                .unwrap()
                .is_some()
        );
        let sent = f.executor.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].task_key, f.child.stable_key());
        assert_eq!(sent[0].request.url, "https://chatgpt.com/c/existing-worker");
        assert!(matches!(
            sent[0].request.prompt,
            ActionPrompt::OfflineCombined { .. }
        ));
        assert_eq!(
            f.runtime
                .store(&f.parent)
                .unwrap()
                .1
                .load(&f.parent)
                .unwrap()
                .agents
                .children
                .len(),
            2
        );
    }
}

#[tokio::test]
async fn parent_message_cannot_override_an_owner_stop_during_delivery() {
    let f = ParentMessageFixture::new().await;
    f.pause("first-pause").await;
    let store = Arc::new(f.runtime.store(&f.child).unwrap().1);
    let snapshot = store.load(&f.child).unwrap();
    // The owner commits the goal stop before its cursor stop flag. A stale
    // parent-message snapshot must not reverse that in-progress owner action.
    store
        .pause_goal_for_agent_stop(&f.child, "new-owner-stop", 200)
        .unwrap();
    let chat = f.chat(&f.child);
    assert!(
        chat.append_parent_instruction(
            "stale-parent".into(),
            "Do not override the owner's stop".into(),
            store.clone(),
            f.child.clone(),
            f.parent.clone(),
            snapshot
        )
        .await
        .is_err()
    );
    assert_eq!(
        store.load(&f.child).unwrap().goal.unwrap().status,
        GoalStatus::Configured
    );
    assert!(chat.agent_pause_requested().await.unwrap());
    assert!(chat.unread_snapshot().await.unwrap().messages.is_empty());
}
