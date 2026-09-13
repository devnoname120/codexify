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
    let (wait, send) = tokio::join!(
        client.call_tool(request("chat_await", json!({}), "widget-owner")),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            client
                .call_tool(request(
                    "chat_ui_send",
                    json!({"request_id":"two", "message":"Reply during await"}),
                    "widget-owner",
                ))
                .await
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
    let second_end = payload(&send.unwrap())["sent"]["end"].as_u64().unwrap();
    let state = client
        .call_tool(request("chat_ui_state", json!({}), "widget-owner"))
        .await
        .unwrap();
    assert_eq!(payload(&state)["delivered_through"], second_end);
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
