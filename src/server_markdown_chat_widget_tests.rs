#[tokio::test]
async fn markdown_chat_widget_send_poll_and_agent_delivery_are_separate() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.markdown_chat.max_wait_ms = 5000;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("widget-owner").unwrap();
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
    for _ in 0..2 {
        let state = client
            .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
            .await
            .unwrap();
        assert_eq!(payload(&state)["delivered_through"], 0);
        assert!(payload(&state)["last_agent_call_at_ms"].is_null());
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
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (wait, send) = tokio::join!(
        client.call_tool(request("chat_await", json!({}), "widget-owner")),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let during = client
                .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
                .await
                .unwrap();
            let started = payload(&during)["last_agent_call_at_ms"].as_u64().unwrap();
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
                let mut config = crate::config::default_config(root.path().into());
                config.ui_widgets = widgets;
                config.markdown_chat.enabled = enabled;
                config.conversation_auth_token = auth.then(|| "a".repeat(64).into());
                config.multi_project = true;
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
async fn markdown_chat_private_selection_does_not_deliver_or_fake_activity() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mut config = crate::config::default_config(root.path().into());
    config.multi_project = true;
    config.markdown_chat.enabled = true;
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
    let sent = client
        .call_tool(request(
            "chat_ui_send",
            json!({"request_id":"private-send", "message":"Only the agent should consume this"}),
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
    assert_eq!(page["delivered_through"], 0);
    assert!(page["read_through"].as_u64().unwrap() < page["messages"][0]["end"].as_u64().unwrap());
    client.cancel().await.unwrap();
    task.await.unwrap();
}
