#[tokio::test]
async fn markdown_chat_widget_send_poll_and_agent_delivery_are_separate() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.markdown_chat.max_wait_ms = 5000;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("widget-owner").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    bind_fixture_workspace(
        &handler,
        Some(&ConversationIdentity::from_openai_session("different-conversation").unwrap()),
    )
    .await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, args: Value, conversation: &str| {
        let mut args = args;
        if name.starts_with("chat_ui_") && name != "chat_ui_state" {
            args.as_object_mut()
                .unwrap()
                .entry("expected_chat_file")
                .or_insert(json!(chat.path()));
        }
        let mut request = CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":conversation})).unwrap());
        request
    };
    let payload = |result: &rmcp::model::CallToolResult| {
        result
            .meta
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat_ui::CHAT_WIDGET_META)
            .unwrap()
            .clone()
    };
    let sent = client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"one", "message":"Widget user input"}),
            "widget-owner",
        ))
        .await
        .unwrap();
    assert_ne!(sent.is_error, Some(true));
    assert!(
        sent.structured_content
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    );
    let end = payload(&sent)["sent"]["end"].as_u64().unwrap();
    assert_eq!(payload(&sent)["sent"]["tool_call_count"], 0);
    for _ in 0..2 {
        let state = client
            .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
            .await
            .unwrap();
        assert_eq!(payload(&state)["delivered_through"], 0);
        assert!(payload(&state)["last_agent_call_at_ms"].is_null());
        assert_eq!(payload(&state)["total_tool_calls"], 0);
        assert_eq!(payload(&state)["messages"][0]["tool_call_count"], 0);
        assert!(payload(&state)["read_through"].as_u64().unwrap() < end);
        assert!(
            state
                .structured_content
                .as_ref()
                .unwrap()
                .get(crate::markdown_chat::USER_MESSAGE_FIELD)
                .is_none()
        );
        assert!(
            chat.read(false)
                .await
                .unwrap()
                .text
                .contains("Widget user input")
        );
    }
    let other = client
        .call_tool(request(
            "chat_ui_state",
            json!({}),
            "different-conversation",
        ))
        .await
        .unwrap();
    assert_eq!(payload(&other)["messages"], json!([]));
    assert!(payload(&other)["last_agent_call_at_ms"].is_null());
    assert_eq!(payload(&other)["total_tool_calls"], 0);
    let returned = client
        .call_tool(request("clock_curr_time", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert!(
        returned.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
            .as_str()
            .unwrap()
            .contains("Widget user input")
    );
    let state = client
        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert_eq!(payload(&state)["delivered_through"], end);
    assert!(
        payload(&state)["read_through"]
            .as_u64()
            .expect("read cursor")
            < end
    );
    let last_call = payload(&state)["last_agent_call_at_ms"]
        .as_u64()
        .expect("agent activity timestamp");
    assert_eq!(payload(&state)["total_tool_calls"], 1);
    assert!(last_call > 0);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let polled = client
        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert_eq!(
        payload(&polled)["last_agent_call_at_ms"],
        last_call,
        "widget polling must not record agent activity"
    );
    assert_eq!(payload(&polled)["total_tool_calls"], 1);
    assert!(
        chat.read(false)
            .await
            .unwrap()
            .text
            .contains("Widget user input")
    );
    client
        .call_tool(request("chat_read", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert!(chat.read(false).await.unwrap().text.is_empty());
    let read_state = client
        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert_eq!(payload(&read_state)["read_through"], end);
    let previous_call = payload(&read_state)["last_agent_call_at_ms"]
        .as_u64()
        .unwrap();
    assert_eq!(payload(&read_state)["total_tool_calls"], 2);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (wait, send) = tokio::join!(
        client.call_tool(request("chat_await", json!({}), "widget-owner")),
        async {
            let during = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let state = client
                        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
                        .await
                        .unwrap();
                    if payload(&state)["input_required"] == true
                        && payload(&state)["agent_waiting_until_ms"].is_u64()
                    {
                        break state;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let started = payload(&during)["last_agent_call_at_ms"].as_u64().unwrap();
            assert_eq!(payload(&during)["total_tool_calls"], 3);
            assert!(payload(&during)["agent_waiting_until_ms"].as_u64().unwrap() > started);
            assert!(
                started > previous_call,
                "a pending wait counts at invocation, not completion"
            );
            let sent = client
                .call_tool(request(
                    "chat_ui_send",
                    json!({"request_id":"two", "message":"Reply during await"}),
                    "widget-owner",
                ))
                .await;
            (sent, started)
        }
    );
    let wait = wait.unwrap();
    assert_eq!(
        wait.structured_content.as_ref().unwrap()["status"],
        "message"
    );
    assert!(
        wait.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
            .as_str()
            .unwrap()
            .contains("Reply during await")
    );
    let (send, wait_started) = send;
    let second_end = payload(&send.unwrap())["sent"]["end"].as_u64().unwrap();
    let state = client
        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert_eq!(payload(&state)["delivered_through"], second_end);
    assert_eq!(payload(&state)["read_through"], second_end);
    assert_eq!(
        payload(&state)["last_agent_call_at_ms"],
        wait_started,
        "completion must not reset the activity clock"
    );
    assert_eq!(payload(&state)["total_tool_calls"], 3);
    assert!(payload(&state)["agent_waiting_until_ms"].is_null());
    assert!(chat.read(false).await.unwrap().text.is_empty());
    for (number, (name, args)) in [
        ("chat_read", json!({})),
        ("chat_write", json!({"message":"Agent response"})),
        ("chat_read", json!({"invalid":true})),
    ]
    .into_iter()
    .enumerate()
    {
        let sent = client.call_tool(request(
            "chat_ui_send",
            json!({"request_id":format!("receipt-{number}"), "message":format!("Receipt check {number}")}),
            "widget-owner",
        )).await.unwrap();
        let end = payload(&sent)["sent"]["end"].as_u64().unwrap();
        let returned = client
            .call_tool(request(name, args, "widget-owner"))
            .await
            .unwrap();
        assert!(
            returned.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
                .as_str()
                .unwrap()
                .contains(&format!("Receipt check {number}"))
        );
        let state = client
            .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
            .await
            .unwrap();
        assert_eq!(payload(&state)["total_tool_calls"], 4 + number);
        if name == "chat_write" {
            let state_payload = payload(&state);
            let agent = state_payload["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|message| message["role"] == "agent")
                .unwrap();
            assert_eq!(agent["tool_call_count"], 5);
        }
        assert!(payload(&state)["delivered_through"].as_u64().unwrap() >= end);
        if number == 2 {
            assert!(
                payload(&state)["read_through"].as_u64().unwrap() < end,
                "invalid chat calls deliver but do not acknowledge input"
            );
        } else {
            assert!(payload(&state)["read_through"].as_u64().unwrap() >= end);
        }
    }
    chat.read(true).await.unwrap();
    assert!(matches!(
        chat.wait(
            Duration::from_millis(1),
            tokio_util::sync::CancellationToken::new()
        )
        .await
        .unwrap(),
        crate::markdown_chat::WaitOutcome::TimedOut(_)
    ));
    let grace = chat
        .widget_page(None, None)
        .await
        .unwrap()
        .agent_waiting_until_ms
        .unwrap();
    for (name, args) in [
        ("chat_read", json!({})),
        ("chat_write", json!({"message":"Still awaiting input"})),
        ("chat_ui_state", json!({})),
    ] {
        client
            .call_tool(request(name, args, "widget-owner"))
            .await
            .unwrap();
        assert_eq!(
            chat.widget_page(None, None)
                .await
                .unwrap()
                .agent_waiting_until_ms,
            Some(grace)
        );
    }
    client
        .call_tool(request(
            "get_environment",
            json!({}),
            "different-conversation",
        ))
        .await
        .unwrap();
    assert_eq!(
        chat.widget_page(None, None)
            .await
            .unwrap()
            .agent_waiting_until_ms,
        Some(grace)
    );
    client
        .call_tool(request("get_environment", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert!(
        chat.widget_page(None, None)
            .await
            .unwrap()
            .agent_waiting_until_ms
            .is_none()
    );
    let forged = client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"three", "message":"No path", "path":chat.path()}),
            "different-conversation",
        ))
        .await
        .unwrap();
    assert_eq!(forged.is_error, Some(true));
    assert!(
        forged
            .structured_content
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    );
    client.cancel().await.unwrap();
    task.await.unwrap();
    let audit = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
    assert!(!audit.contains("Widget user input"));
    assert!(!audit.contains("Reply during await"));
}

#[test]
fn markdown_chat_only_setup_advertises_a_chat_template() {
    let root = tempfile::tempdir().unwrap();
    for widgets in [false, true] {
        for enabled in [false, true] {
            for auth in [false, true] {
                let mut config =
                    crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
                config.ui_widgets = widgets;
                config.markdown_chat.enabled = enabled;
                config.conversation_auth_token = auth.then(|| "a".repeat(64).into());

                let tools = crate::registry::load_tools_for_config(&config);
                let mut linked = Vec::new();
                for tool in &tools {
                    let meta = serde_json::to_value(advertised_tool(tool.as_ref(), &config))
                        .unwrap()["_meta"]
                        .clone();
                    if meta["ui"]["resourceUri"] == crate::markdown_chat_ui::SETUP_CHAT_UI_URI {
                        linked.push(tool.name());
                    }
                    if ["chat_read", "chat_write", "chat_await"].contains(&tool.name()) {
                        assert!(meta["ui"].get("resourceUri").is_none());
                        assert!(meta.get("openai/outputTemplate").is_none());
                    }
                    if tool.name().starts_with("setup_ui_") {
                        assert!(app_only_tool(tool.as_ref()));
                    }
                }
                assert_eq!(
                    linked,
                    if enabled && widgets {
                        vec!["setup"]
                    } else {
                        vec![]
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn only_model_visible_calls_start_offline_notification_monitors() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.markdown_chat.notifications = Some(crate::markdown_chat::NotificationsConfig {
        urls: vec!["ntfys://ntfy.example/topic?image=no".into()],
        subagents: true,
        python_path: root.path().join("missing-python").display().to_string(),
        timeout_ms: 1000,
        events: Default::default(),
    });
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config);
    let chat_store = handler.markdown_chat.clone();
    bind_fixture_workspace(&handler, None).await;
    bind_fixture_workspace(
        &handler,
        Some(&ConversationIdentity::from_openai_session("first").unwrap()),
    )
    .await;
    bind_fixture_workspace(
        &handler,
        Some(&ConversationIdentity::from_openai_session("second").unwrap()),
    )
    .await;
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, conversation: &str| {
        let mut request = CallToolRequestParams::new(name.to_string())
            .with_arguments(json!({}).as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":conversation})).unwrap());
        request
    };

    client
        .call_tool(request("chat_ui_state", "first"))
        .await
        .unwrap();
    assert_eq!(chat_store.offline_monitor_count(), 0);
    client
        .call_tool(request("clock_curr_time", "first"))
        .await
        .unwrap();
    assert_eq!(chat_store.offline_monitor_count(), 1);
    client
        .call_tool(request("chat_ui_state", "first"))
        .await
        .unwrap();
    assert_eq!(chat_store.offline_monitor_count(), 1);
    client
        .call_tool(request("clock_curr_time", "second"))
        .await
        .unwrap();
    assert_eq!(chat_store.offline_monitor_count(), 2);

    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn markdown_chat_private_selection_does_not_deliver_or_fake_activity() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());

    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, args: Value| {
        let mut request = CallToolRequestParams::new(name.to_string())
            .with_arguments(args.as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"private-selector"})).unwrap());
        request
    };
    let selected = client
        .call_tool(request(
            crate::tools::setup_ui_action::SELECT_NAME,
            json!({"path":project,"createWorktree":false}),
        ))
        .await
        .unwrap();
    assert_ne!(selected.is_error, Some(true), "{selected:?}");
    assert_eq!(
        selected.structured_content.as_ref().unwrap()["active_root"],
        json!(std::fs::canonicalize(&project).unwrap())
    );
    let initial = client
        .call_tool(request("chat_ui_state", json!({})))
        .await
        .unwrap();
    let initial = &initial.meta.as_ref().unwrap()[crate::markdown_chat_ui::CHAT_WIDGET_META];
    let sent = client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"private-send", "message":"Only the agent should consume this", "expected_chat_file":initial["chat_file"], "expected_workspace":initial["workspace_path"]}),
        ))
        .await
        .unwrap();
    assert_ne!(sent.is_error, Some(true));
    for name in ["setup_ui_list_projects", "chat_ui_state"] {
        let result = client.call_tool(request(name, json!({}))).await.unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert!(
            result
                .structured_content
                .as_ref()
                .unwrap()
                .get(crate::markdown_chat::USER_MESSAGE_FIELD)
                .is_none()
        );
    }
    let state = client
        .call_tool(request("chat_ui_state", json!({})))
        .await
        .unwrap();
    let page = state
        .meta
        .as_ref()
        .unwrap()
        .get(crate::markdown_chat_ui::CHAT_WIDGET_META)
        .unwrap();
    assert!(page["last_agent_call_at_ms"].is_null());
    assert_eq!(page["total_tool_calls"], 0);
    assert_eq!(page["delivered_through"], 0);
    assert!(page["read_through"].as_u64().unwrap() < page["messages"][0]["end"].as_u64().unwrap());
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn widget_stop_is_opportunistic_and_polling_cannot_restore_stopped_presence() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.markdown_chat.automation.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("stop-widget-owner").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    let store = crate::agent_automation::AgentAutomationStore::for_config(&config).unwrap();
    store
        .apply_owner_goal_command(
            &identity,
            "initial-goal",
            crate::agent_automation::OwnerGoalCommand::Set {
                markdown: "Finish this goal".into(),
            },
            1000,
        )
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, args: Value| {
        let mut args = args;
        if name.starts_with("chat_ui_") {
            args.as_object_mut()
                .unwrap()
                .entry("expected_chat_file")
                .or_insert(json!(chat.path()));
        }
        let mut req = CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().unwrap().clone());
        req.meta =
            Some(serde_json::from_value(json!({"openai/session":"stop-widget-owner"})).unwrap());
        req
    };
    let stop = client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"stop-once", "message":"ignored", "stop_agent":true}),
        ))
        .await
        .unwrap();
    assert_ne!(stop.is_error, Some(true));
    assert_eq!(
        store.load(&identity).unwrap().goal.unwrap().status,
        crate::agent_automation::GoalStatus::Configured
    );
    assert!(
        stop.structured_content
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    );
    assert!(chat.clean_markdown_transcript().await.unwrap().is_empty());
    let result = client
        .call_tool(request(
            "chat_write",
            json!({"message":"My current progress and status."}),
        ))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD],
        crate::markdown_chat::AGENT_STOP_MESSAGE
    );
    for _ in 0..2 {
        let result = client
            .call_tool(request("chat_ui_state", json!({})))
            .await
            .unwrap();
        let page = &result.meta.as_ref().unwrap()[crate::markdown_chat_ui::CHAT_WIDGET_META];
        assert!(page["last_agent_call_at_ms"].is_null());
        assert_eq!(page["agent_stop_requested"], true);
        assert_eq!(page["total_tool_calls"], 1);
    }
    client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"resume-message", "message":"Another instruction"}),
        ))
        .await
        .unwrap();
    for _ in 0..2 {
        let result = client
            .call_tool(request("chat_ui_state", json!({})))
            .await
            .unwrap();
        let page = &result.meta.as_ref().unwrap()[crate::markdown_chat_ui::CHAT_WIDGET_META];
        assert!(
            page["last_agent_call_at_ms"].is_null(),
            "old activity replay must not restore online after the user resumes"
        );
        assert_eq!(page["agent_stop_requested"], false);
    }
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn private_queue_is_excluded_from_passive_delivery_and_completed_calls_release_one() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.markdown_chat.max_wait_ms = 1000;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("queue-dispatch").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    chat.record_agent_call(crate::markdown_chat::now_ms())
        .await
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, args: Value| {
        let mut args = args;
        if name.starts_with("chat_ui_") {
            args.as_object_mut()
                .unwrap()
                .entry("expected_chat_file")
                .or_insert(json!(chat.path()));
        }
        let mut request = CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"queue-dispatch"})).unwrap());
        request
    };
    for id in ["first", "second"] {
        let result=client.call_tool(request("chat_ui_send",json!({"request_id":id,"message":format!("secret queued {id}"),"message_mode":"queue"}))).await.unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(
            !result
                .structured_content
                .as_ref()
                .unwrap()
                .to_string()
                .contains("secret queued")
        );
    }
    for (name, args) in [
        ("chat_read", json!({})),
        ("get_agent_brief", json!({})),
        ("chat_await", json!({})),
    ] {
        let result = client.call_tool(request(name, args)).await.unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret queued")
        );
    }
    let first = client
        .call_tool(request("turn_completed", json!({})))
        .await
        .unwrap();
    assert_ne!(first.is_error, Some(true));
    assert!(
        first.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
            .as_str()
            .unwrap()
            .contains("secret queued first")
    );
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("secret queued second")
    );
    let repeat = client
        .call_tool(request("turn_completed", json!({})))
        .await
        .unwrap();
    assert_eq!(
        repeat.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_write"
    );
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    client
        .call_tool(request(
            "chat_write",
            json!({"message":"Handled first request"}),
        ))
        .await
        .unwrap();
    let second = client
        .call_tool(request("turn_completed", json!({})))
        .await
        .unwrap();
    assert!(
        second.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
            .as_str()
            .unwrap()
            .contains("secret queued second")
    );
    assert!(chat.queued_messages().await.unwrap().is_empty());
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rollout_reminders_count_model_work_and_recovery_tools_preserve_private_queue() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.rollout_reminder_interval = 20;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.markdown_chat.max_wait_ms = 1000;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("rollout-dispatch").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    for name in ["chat_await", "turn_completed", "goal_finish"] {
        for _ in 0..25 {
            let activity = handler
                .record_chat_activity(Some(&identity), None, crate::markdown_chat::now_ms(), name)
                .await;
            assert!(
                activity.is_some(),
                "waiting still records normal agent activity"
            );
        }
    }
    assert!(
        chat.rollout_reminder(20).await.unwrap().is_none(),
        "25 waiting/completion calls per tool must not advance the progress counter"
    );
    std::fs::write(
        chat.path().with_file_name("automation.json"),
        r#"{"goal":{"status":"blocked"}}"#,
    )
    .unwrap();
    chat.queue_user_message(
        "hidden".into(),
        "Private queued work must stay hidden".into(),
        vec![],
    )
    .await
    .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let request = |name: &str, args: Value| {
        let mut request = CallToolRequestParams::new(name.to_owned())
            .with_arguments(args.as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"rollout-dispatch"})).unwrap());
        request
    };
    let field = crate::markdown_chat::rollout::REMINDER_FIELD;
    // A due checkpoint from earlier work must remain pending while the agent
    // waits or completes a goal, rather than being attached to those results.
    for _ in 0..20 {
        chat.record_rollout_work().await.unwrap();
    }
    for name in ["chat_await", "turn_completed", "goal_finish"] {
        let response = client.call_tool(request(name, json!({}))).await.unwrap();
        assert!(
            response
                .structured_content
                .as_ref()
                .is_none_or(|body| body.get(field).is_none()),
            "waiting/completion results must not request a progress checkpoint: {name}"
        );
    }
    let waiting_state: Value = serde_json::from_str(
        &std::fs::read_to_string(chat.path().with_file_name("rollout-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(waiting_state["work_calls"], 20);
    assert_eq!(waiting_state["reminded_calls"], 0);
    let work = client
        .call_tool(request("chat_read", json!({})))
        .await
        .unwrap();
    assert!(
        work.structured_content
            .as_ref()
            .unwrap()
            .get(field)
            .is_some(),
        "the next work result must still deliver the pending checkpoint"
    );
    chat.append_rollout(
        "initial-checkpoint".into(),
        "Working".into(),
        "Saved prior work".into(),
        "Continue verification".into(),
    )
    .await
    .unwrap();
    for count in 1..=20 {
        let result = client
            .call_tool(request("chat_read", json!({})))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let body = result.structured_content.as_ref().unwrap();
        assert_eq!(body.get(field).is_some(), count == 20);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("Private queued work")
        );
        let ui = client
            .call_tool(request("chat_ui_state", json!({})))
            .await
            .unwrap();
        assert!(ui.structured_content.as_ref().unwrap().get(field).is_none());
    }
    for _ in 0..2 {
        let wait = client
            .call_tool(request("chat_await", json!({})))
            .await
            .unwrap();
        assert!(
            wait.structured_content
                .as_ref()
                .unwrap()
                .get(field)
                .is_none()
        );
    }
    let saved=client.call_tool(request("rollout_update",json!({"request_id":"checkpoint-1","status":"Working","progress":"Completed verification","next_steps":"Continue the goal once input arrives"}))).await.unwrap();
    assert_ne!(saved.is_error, Some(true));
    assert!(
        saved
            .structured_content
            .as_ref()
            .unwrap()
            .get(field)
            .is_none()
    );
    let read = client
        .call_tool(request("rollout_read", json!({})))
        .await
        .unwrap();
    assert_ne!(read.is_error, Some(true));
    assert!(
        read.structured_content.as_ref().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("Completed verification")
    );
    assert!(
        !serde_json::to_string(&read)
            .unwrap()
            .contains("Private queued work")
    );
    assert_eq!(chat.queued_messages().await.unwrap().len(), 1);
    for index in 1..=19 {
        let result = client
            .call_tool(request("chat_read", json!({})))
            .await
            .unwrap();
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .unwrap()
                .get(field)
                .is_some(),
            index == 19,
            "rollout_read is also a model work call"
        );
    }
    let brief = client
        .call_tool(request("get_agent_brief", json!({})))
        .await
        .unwrap();
    let text = serde_json::to_string(&brief).unwrap();
    assert!(
        text.contains("rollout.md")
            && text.contains("rollout_read")
            && text.contains("rollout_update")
    );
    std::fs::write(chat.path().with_file_name("rollout-state.json"), "{").unwrap();
    let successful = client
        .call_tool(request("chat_read", json!({})))
        .await
        .unwrap();
    assert_ne!(successful.is_error, Some(true));
    assert!(
        successful.structured_content.as_ref().unwrap()[field]
            .as_str()
            .unwrap()
            .contains("original tool operation")
    );
    chat.request_agent_stop("stop-before-checkpoint".into())
        .await
        .unwrap();
    let stopped = client
        .call_tool(request("chat_read", json!({})))
        .await
        .unwrap();
    assert!(
        stopped
            .structured_content
            .as_ref()
            .unwrap()
            .get(field)
            .is_none(),
        "a checkpoint reminder must not override an explicit stop"
    );
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn rollout_recovery_returns_complete_reports_with_a_small_ordinary_tool_budget() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.markdown_chat.default_message_mode = crate::markdown_chat::MessageMode::Steer;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.output.max_tool_output_tokens = Some(20);
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("rollout-budget").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.append_rollout(
        "large-report".into(),
        "Current status".into(),
        "Verified result. ".repeat(400),
        "Finish with the exact remaining work".into(),
    )
    .await
    .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(128 * 1024);
    let task = tokio::spawn(async move {
        handler
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let client = ().serve(client_transport).await.unwrap();
    let mut request =
        CallToolRequestParams::new("rollout_read").with_arguments(serde_json::Map::new());
    request.meta =
        Some(serde_json::from_value(json!({"openai/session":"rollout-budget"})).unwrap());
    let result = client.call_tool(request).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let body = result.structured_content.as_ref().unwrap()["content"]
        .as_str()
        .unwrap();
    assert_eq!(body.matches("Verified result.").count(), 400);
    assert!(body.contains("Finish with the exact remaining work"));
    assert!(!body.contains("truncated"));
    client.cancel().await.unwrap();
    task.await.unwrap();
}
