#[tokio::test]
async fn renewal_control_reaches_real_mcp_results_without_queue_or_progress_dependency() {
    renewal_with_pending_messages(false).await;
}

#[tokio::test]
async fn unread_messages_cannot_starve_due_turn_renewal() {
    renewal_with_pending_messages(true).await;
}

async fn renewal_with_pending_messages(unread: bool) {
    use crate::agent_automation::turn_renewal::TurnSend;
    use crate::agent_automation::{
        AgentAutomationStore, AutomationActionKind, AutomationActionStatus,
    };
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.memory.dir = Some(root.path().join("state").display().to_string());
    config.markdown_chat.enabled = true;
    config.markdown_chat.automation.enabled = true;
    config.markdown_chat.turn_renewal.enabled = true;
    config.markdown_chat.rollout_enabled = false;
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    handler.agent_automation = Some(crate::agent_automation::AgentAutomationSupervisor::new(
        config.markdown_chat.automation.clone(),
        Arc::new(ServerAutomationNoopExecutor),
    ));
    let identity = ConversationIdentity::from_openai_session("mcp-renewal").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let store = AgentAutomationStore::for_config(&config).unwrap();
    let now = crate::markdown_chat::now_ms();
    let pair = store
        .issue_pairing(&identity, "https://chatgpt.com/c/mcp-renewal", 0, now)
        .unwrap();
    store
        .record_pairing_canonical_url(
            &identity,
            pair.generation,
            "https://chatgpt.com/c/mcp-renewal",
            now,
        )
        .unwrap();
    store
        .verify_pairing(&identity, pair.generation, 0, &pair.reference, now)
        .unwrap();
    store
        .queue_action(
            &identity,
            "expired-send",
            AutomationActionKind::OfflineChat,
            pair.generation,
            None,
            now,
        )
        .unwrap();
    store
        .record_turn_send(
            &identity,
            TurnSend {
                action_id: "expired-send".into(),
                at_ms: now - 55 * 60_000,
                model: Some("gpt-6-astra".into()),
                effort: Some("max".into()),
                pairing_generation: pair.generation,
                owner_generation: 0,
            },
        )
        .unwrap();
    store
        .update_action_status(
            &identity,
            "expired-send",
            AutomationActionStatus::Idle,
            None,
            None,
            now,
        )
        .unwrap();
    store.turn_renewal_notice(&identity, now).unwrap();
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.queue_user_message(
        "secret-queue".into(),
        "Do not expose this queued message".into(),
        vec![],
    )
    .await
    .unwrap();
    if unread {
        chat.append_user(
            "pending-owner".into(),
            "Preserve this owner instruction for the next turn".into(),
        )
        .await
        .unwrap();
    }
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let server = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let assert_edit_tools = |tools: &[rmcp::model::Tool]| {
        for name in ["exec_command", "apply_patch", "write_file"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .expect("development tool stays advertised");
            assert_eq!(
                tool.annotations.as_ref().unwrap().read_only_hint,
                Some(false),
                "{name} must retain its real write behavior"
            );
        }
    };
    let initial_tools = client.list_all_tools().await.unwrap();
    assert_edit_tools(&initial_tools);
    for tool in [
        "rollout_read",
        "chat_await",
        "turn_completed",
        "get_agent_brief",
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            client.call_tool(automation_request(tool, json!({}), "mcp-renewal")),
        )
        .await
        .expect("handoff must not wait for user input")
        .unwrap();
        assert_ne!(response.is_error, Some(true), "{tool}: {response:?}");
        let data = response
            .structured_content
            .as_ref()
            .expect("structured MCP result");
        assert!(
            data["turn_renewal_required"]
                .as_str()
                .unwrap()
                .contains("FINAL CHATGPT ASSISTANT ANSWER"),
            "{tool}: {data}"
        );
        if unread && tool == "rollout_read" {
            assert!(
                data["new_chat_message_from_user"]
                    .as_str()
                    .unwrap()
                    .contains("Preserve this owner instruction"),
                "owner messages and renewal must both reach the model"
            );
        }
        assert!(
            data.get("rollout_checkpoint_required")
                .is_none_or(|v| v.is_null())
        );
        assert!(!format!("{response:?}").contains("Do not expose this queued message"));
        if tool == "rollout_read" {
            assert!(data["content"].as_str().unwrap().contains("disabled"));
        }
    }
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    let renewed_tools = client.list_all_tools().await.unwrap();
    assert_eq!(renewed_tools.len(), initial_tools.len());
    assert_edit_tools(&renewed_tools);
    if unread {
        let pending = chat.unread_snapshot().await.unwrap();
        assert_eq!(
            pending.messages.len(),
            1,
            "renewal must not consume owner instructions"
        );
        assert!(
            pending.messages[0]
                .markdown
                .contains("Preserve this owner instruction")
        );
        chat.request_agent_stop("owner-stop".into()).await.unwrap();
        let response = client
            .call_tool(automation_request("rollout_read", json!({}), "mcp-renewal"))
            .await
            .unwrap();
        assert!(
            response
                .structured_content
                .as_ref()
                .unwrap()
                .get("turn_renewal_required")
                .is_none_or(|value| value.is_null()),
            "renewal must respect an explicit stop"
        );
        client.cancel().await.unwrap();
        server.await.unwrap();
        return;
    }
    assert!(chat.unread_snapshot().await.unwrap().messages.is_empty());
    let handoff = store.load(&identity).unwrap().turn_renewal.handoff.unwrap();
    store.confirm_turn_handoff(&identity, &handoff.id).unwrap();
    store
        .queue_action(
            &identity,
            "uncertain-resume",
            AutomationActionKind::TurnRenewal,
            pair.generation,
            None,
            now - 10,
        )
        .unwrap();
    store
        .update_action_status(
            &identity,
            "uncertain-resume",
            AutomationActionStatus::Ambiguous,
            Some("Uncertain browser receipt"),
            None,
            now - 5,
        )
        .unwrap();
    let response = client
        .call_tool(automation_request(
            "get_agent_brief",
            json!({}),
            "mcp-renewal",
        ))
        .await
        .unwrap();
    assert_ne!(response.is_error, Some(true));
    assert!(
        response
            .structured_content
            .as_ref()
            .unwrap()
            .get("turn_renewal_required")
            .is_none_or(|v| v.is_null())
    );
    let after = store.load(&identity).unwrap();
    assert!(after.turn_renewal.handoff.is_none());
    assert!(after.action.last_error.is_none());
    let completed = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_tool(automation_request(
            "turn_completed",
            json!({}),
            "mcp-renewal",
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(completed.is_error, Some(true));
    assert!(
        completed.structured_content.as_ref().unwrap()["new_chat_message_from_user"]
            .as_str()
            .unwrap()
            .contains("Do not expose this queued message")
    );
    client.cancel().await.unwrap();
    server.await.unwrap();
}
