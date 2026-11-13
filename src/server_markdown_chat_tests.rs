struct MarkdownPassthroughFixture {
    name: &'static str,
    schema: Option<Value>,
    data: Option<Value>,
}

#[async_trait]
impl Tool for MarkdownPassthroughFixture {
    fn name(&self) -> &'static str {
        self.name
    }
    fn title(&self) -> String {
        "Passthrough fixture".into()
    }
    fn description(&self) -> String {
        "Test-only upstream result passthrough".into()
    }
    fn behavior(&self) -> crate::tool::ToolBehavior {
        crate::tool::ToolBehavior::new(
            self.name != "exec_command",
            false,
            true,
            false,
            "Test-only passthrough",
        )
    }
    fn input_schema(&self) -> Value {
        crate::tool::empty_object_schema()
    }
    fn output_schema(&self) -> Option<Value> {
        self.schema.clone()
    }
    fn requires_closed_output_schema(&self) -> bool {
        false
    }
    fn permits_missing_structured_content(&self) -> bool {
        true
    }
    fn fills_structured_content(&self) -> bool {
        false
    }
    fn requires_project_root(&self) -> bool {
        false
    }
    async fn call(&self, _: Value, _: &AppConfig, _: &SessionState) -> ToolResult {
        let mut result = ToolResult::image("aW1hZ2U=", "image/png");
        result
            .content
            .push(ToolContent::ResourceLink(rmcp::model::Resource::new(
                "test://resource",
                "fixture",
            )));
        result.structured_content = self.data.clone();
        result.meta = Some(serde_json::from_value(json!({"vendor":"retained"})).unwrap());
        result
    }
}

#[tokio::test]
async fn markdown_chat_bridged_results_and_widget_metadata_keep_their_contents() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    let fixtures = vec![
        MarkdownPassthroughFixture {
            name: "direct_text_only",
            schema: Some(crate::tool::text_output_schema()),
            data: None,
        },
        MarkdownPassthroughFixture {
            name: "gateway",
            schema: None,
            data: Some(json!(["original"])),
        },
        MarkdownPassthroughFixture {
            name: "catalog_call",
            schema: None,
            data: Some(json!({"value":42})),
        },
        MarkdownPassthroughFixture {
            name: "open_schema",
            schema: Some(
                json!({"type":"object", "properties":{"value":{"type":"integer"}}, "required":["value"]}),
            ),
            data: Some(json!({"value":42, "new_chat_message_from_user":true})),
        },
    ];
    let tools: Vec<Box<dyn Tool>> = fixtures
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn Tool>)
        .collect();
    let mut handler = handler_with_tools(root.path(), tools, crate::types::ToolLogLevel::Info);
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("passthrough").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(chat.path())
        .unwrap()
        .write_all(b"new user text")
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
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
    for tool in client.list_all_tools().await.unwrap() {
        let mut request = CallToolRequestParams::new(tool.name.clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"passthrough"})).unwrap());
        let result = client.call_tool(request).await.unwrap();
        assert_ne!(result.is_error, Some(true));
        let data = result.structured_content.as_ref().unwrap();
        assert!(
            data[crate::markdown_chat::USER_MESSAGE_FIELD]
                .as_str()
                .unwrap()
                .ends_with("new user text")
        );
        assert!(jsonschema::is_valid(
            &serde_json::to_value(tool.output_schema.unwrap()).unwrap(),
            data
        ));
        assert!(matches!(
            &result.content[0],
            rmcp::model::ContentBlock::Image(_)
        ));
        assert_eq!(result.meta.unwrap().get("vendor"), Some(&json!("retained")));
        match tool.name.as_ref() {
            "gateway" => assert_eq!(data["upstream_result"], json!(["original"])),
            "catalog_call" => assert_eq!(data["value"], 42),
            "open_schema" => assert_eq!(
                data["upstream_result"][crate::markdown_chat::USER_MESSAGE_FIELD],
                true
            ),
            _ => {}
        }
    }
    client.cancel().await.unwrap();
    task.await.unwrap();
}

#[test]
fn markdown_chat_all_advertised_outputs_have_an_optional_user_message() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;

    config.conversation_auth_token = Some("a".repeat(64).into());
    for tool in crate::registry::load_tools_for_config(&config) {
        let advertised = advertised_tool(tool.as_ref(), &config);
        let schema = serde_json::to_value(advertised.output_schema).unwrap();
        assert_eq!(
            schema["properties"][crate::markdown_chat::USER_MESSAGE_FIELD]["type"],
            "string",
            "{}",
            tool.name()
        );
        assert!(!schema["required"].as_array().is_some_and(|fields| {
            fields.contains(&json!(crate::markdown_chat::USER_MESSAGE_FIELD))
        }));
    }
    config.markdown_chat.enabled = false;
    for tool in crate::registry::load_tools_for_config(&config) {
        let advertised = advertised_tool(tool.as_ref(), &config);
        let schema = serde_json::to_value(advertised.output_schema).unwrap();
        assert!(
            schema["properties"]
                .get(crate::markdown_chat::USER_MESSAGE_FIELD)
                .is_none()
        );
        if !app_only_tool(tool.as_ref()) {
            assert_eq!(
                schema["properties"][WORKSPACE_CHANGE_FIELD]["type"],
                "string"
            );
        } else {
            assert_eq!(schema, json!(tool.output_schema()));
        }
    }
}

#[tokio::test]
async fn markdown_chat_dispatch_delivers_full_messages_after_budgeting_and_errors() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("example.txt"), "source\n").unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.output.max_tool_output_tokens = Some(100);
    let mut tools = crate::registry::load_tools_for_config(&config);
    tools.push(Box::new(InvalidStructuredOutputTool));
    let mut handler = handler_with_tools(root.path(), tools, crate::types::ToolLogLevel::Info);
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("chat-dispatch").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
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
            Some(serde_json::from_value(json!({"openai/session":"chat-dispatch"})).unwrap());
        request
    };
    let initial = client
        .call_tool(request("get_environment", json!({})))
        .await
        .unwrap();
    assert!(
        chat.path().is_file(),
        "first authorized project call must create CHAT.md"
    );
    assert!(
        initial
            .structured_content
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    );
    let message = format!("{}END", "User Markdown\n".repeat(20_000));
    std::fs::OpenOptions::new()
        .append(true)
        .open(chat.path())
        .unwrap()
        .write_all(message.as_bytes())
        .unwrap();
    for (name, args) in [
        ("read_file", json!({"path":"example.txt"})),
        ("read_file", json!({"path":"missing.txt"})),
        ("read_file", json!({"unexpected":true})),
        ("chat_read", json!({"unexpected":true})),
        ("chat_write", json!({"message":""})),
        ("chat_await", json!({"maxWaitMs":1})),
        ("invalid_structured_output_fixture", json!({})),
        ("clock_curr_time", json!({})),
        ("show_diff", json!({})),
    ] {
        let result = client.call_tool(request(name, args)).await.unwrap();
        let field =
            &result.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD];
        assert!(field.as_str().unwrap().ends_with(&message), "{name}");
        assert!(
            result
                .content
                .iter()
                .any(|block| block
                    .as_text()
                    .is_some_and(|text| serde_json::from_str::<Value>(&text.text)
                        .ok()
                        .is_some_and(|v| v[crate::markdown_chat::USER_MESSAGE_FIELD] == *field)))
        );
        assert_eq!(chat.read(false).await.unwrap().text, message);
    }
    let consumed = client
        .call_tool(request("chat_write", json!({"message":"Agent response"})))
        .await
        .unwrap();
    assert!(
        consumed.structured_content.unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD]
            .as_str()
            .unwrap()
            .contains("Before you wrote")
    );
    let empty = client
        .call_tool(request("chat_read", json!({})))
        .await
        .unwrap();
    assert!(
        empty
            .structured_content
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    );
    assert!(chat.read(false).await.unwrap().text.is_empty());
    client.cancel().await.unwrap();
    task.await.unwrap();
    let audit = std::fs::read_to_string(root.path().join("audit.jsonl")).unwrap();
    assert!(!audit.contains("User Markdown"));
    assert!(!audit.contains("Agent response"));
}

#[tokio::test]
async fn markdown_chat_never_initializes_or_exposes_a_channel_before_setup() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::default_config(std::fs::canonicalize(root.path()).unwrap());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.conversation_auth_token = Some("a".repeat(64).into());
    let mut handler = handler_with_tools(
        root.path(),
        crate::registry::load_tools_for_config(&config),
        crate::types::ToolLogLevel::Info,
    );
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("unauthorized").unwrap();
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
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
    for (name, args) in [
        ("chat_read", json!({})),
        ("chat_ui_state", json!({})),
        ("setup_ui_update", json!({"confirm":false})),
        (
            "chat_ui_send",
            json!({"request_id":"unauthorized", "message":"Not saved"}),
        ),
        ("setup", json!({"ref":"b".repeat(64)})),
    ] {
        let mut request =
            CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone());
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"unauthorized"})).unwrap());
        let response = client.call_tool(request).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        assert!(!chat.path().exists());
        assert!(
            response
                .structured_content
                .as_ref()
                .is_none_or(|v| v.get(crate::markdown_chat::USER_MESSAGE_FIELD).is_none())
        );
    }
    client.cancel().await.unwrap();
    task.await.unwrap();
}

fn pending_chat_context(
    handler: &CodexHandler,
    identity: &ConversationIdentity,
    cancellation: CancellationToken,
) -> ToolRequestContext {
    ToolRequestContext {
        conversation: Some(identity.clone()),
        task_conversation: Some(identity.clone()),
        task_owner_generation: Some(0),
        conversation_retired: false,
        connector_schema_version: None,
        conversation_schema_version: None,
        markdown_chat: handler.markdown_chat.clone(),
        conversation_authorizations: handler.conversation_authorizations.clone(),
        project_bindings: handler.project_bindings.clone(),
        diff_checkpoints: handler.diff_checkpoints.clone(),
        artifact_egress: handler.artifact_egress.clone(),
        agent_automation: None,
        agent_runtime: None,
        cancellation,
    }
}

#[tokio::test]
async fn inline_pending_chat_read_delivers_complete_input_then_acknowledges_once_and_keeps_late_input()
 {
    let root = tempfile::tempdir().unwrap();
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(MarkdownPassthroughFixture {
            name: "exec_command",
            schema: Some(crate::tool::text_output_schema()),
            data: None,
        }),
        Box::new(crate::tools::markdown_chat::ChatTool::Read),
        Box::new(crate::tools::markdown_chat::ChatTool::Write),
    ];
    let mut handler = handler_with_tools(root.path(), tools, crate::types::ToolLogLevel::Info);
    let mut config = handler.config.as_ref().clone();
    config.markdown_chat.enabled = true;
    config.output.max_tool_output_tokens = Some(128);
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("inline-inbox").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let other_identity = ConversationIdentity::from_openai_session("other-inline-inbox").unwrap();
    bind_fixture_workspace(&handler, Some(&other_identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    let message = format!(
        "{}END_OF_COMPLETE_INPUT",
        "Owner instruction\n".repeat(20_000)
    );
    chat.append_user("inline-owner".into(), message.clone())
        .await
        .unwrap();
    let expected_input = chat.read(false).await.unwrap().text;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
    let request = |name: &str, args: Value| {
        let mut request = CallToolRequestParams::new(name.to_owned());
        request.arguments = args.as_object().cloned();
        request.meta =
            Some(serde_json::from_value(json!({"openai/session":"inline-inbox"})).unwrap());
        request
    };
    let invalid = client
        .call_tool(request("exec_command", json!({"unexpected":true})))
        .await
        .unwrap();
    assert_eq!(invalid.is_error, Some(true));
    assert!(
        !chat
            .user_messages_read(&["inline-owner".into()])
            .await
            .unwrap(),
        "argument rejection never consumes input"
    );
    let mut other_request = request("exec_command", json!({}));
    other_request.meta =
        Some(serde_json::from_value(json!({"openai/session":"other-inline-inbox"})).unwrap());
    let other_result = client.call_tool(other_request).await.unwrap();
    assert_ne!(other_result.is_error, Some(true));
    assert!(
        other_result
            .structured_content
            .as_ref()
            .is_none_or(|value| value
                .get(crate::markdown_chat::USER_MESSAGE_FIELD)
                .is_none())
    );
    assert!(
        !chat
            .user_messages_read(&["inline-owner".into()])
            .await
            .unwrap(),
        "another conversation cannot receive or consume this inbox"
    );
    let blocked = client
        .call_tool(request("exec_command", json!({})))
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert!(
        !blocked
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_))),
        "the requested operation never dispatched"
    );
    assert!(
        blocked.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Required next tool: chat_write")
    );
    let field =
        &blocked.structured_content.as_ref().unwrap()[crate::markdown_chat::USER_MESSAGE_FIELD];
    assert!(
        field
            .as_str()
            .unwrap()
            .contains("Required next tool: chat_write"),
        "the next action stays beside the unbudgeted complete input"
    );
    assert!(
        field.as_str().unwrap().ends_with(&expected_input),
        "complete input bypasses the ordinary 128-token work-result budget"
    );
    assert!(
        blocked
            .content
            .iter()
            .any(|block| block
                .as_text()
                .is_some_and(
                    |text| serde_json::from_str::<Value>(&text.text).ok().is_some_and(
                        |value| value[crate::markdown_chat::USER_MESSAGE_FIELD] == *field
                    )
                )),
        "text-only hosts receive the exact same receipt"
    );
    assert!(
        chat.user_messages_read(&["inline-owner".into()])
            .await
            .unwrap(),
        "same read cursor semantics as explicit chat_read"
    );
    assert!(
        !std::fs::read_to_string(chat.path())
            .unwrap()
            .contains("codexify-agent-message:v1:start"),
        "inline reading never appends an acknowledgment or claims the direction handled"
    );
    let acknowledgment = client
        .call_tool(request(
            "chat_write",
            json!({"message":"Acknowledged; applying the changed plan."}),
        ))
        .await
        .unwrap();
    assert_ne!(acknowledgment.is_error, Some(true));
    assert!(
        acknowledgment
            .structured_content
            .as_ref()
            .unwrap()
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none(),
        "one acknowledgment does not echo already-read input and demand another"
    );
    let worked = client
        .call_tool(request("exec_command", json!({})))
        .await
        .unwrap();
    assert_ne!(worked.is_error, Some(true));
    assert!(matches!(&worked.content[0], ContentBlock::Image(_)));

    chat.append_user("second-owner".into(), "Second direction".into())
        .await
        .unwrap();
    let blocked = client
        .call_tool(request("exec_command", json!({})))
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert!(
        !blocked
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    );
    chat.append_user(
        "late-owner".into(),
        "Late direction after inline receipt".into(),
    )
    .await
    .unwrap();
    let acknowledgment = client
        .call_tool(request(
            "chat_write",
            json!({"message":"Acknowledged the second direction."}),
        ))
        .await
        .unwrap();
    let late = acknowledgment.structured_content.as_ref().unwrap()
        [crate::markdown_chat::USER_MESSAGE_FIELD]
        .as_str()
        .unwrap();
    assert!(late.contains("Late direction after inline receipt"));
    assert!(!late.contains("Second direction"));
    assert_eq!(
        acknowledgment.structured_content.as_ref().unwrap()["required_next_action"],
        "chat_write"
    );
    assert!(
        chat.user_messages_read(&["late-owner".into()])
            .await
            .unwrap()
    );
    chat.request_agent_stop("explicit-stop".into())
        .await
        .unwrap();
    let stopped = client
        .call_tool(request("exec_command", json!({})))
        .await
        .unwrap();
    assert_eq!(stopped.is_error, Some(true));
    assert!(
        !stopped
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    );
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn inline_pending_chat_read_oversize_inbox_stays_unread_and_requires_explicit_read() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let mut handler = handler_with_tools(
        root.path(),
        vec![Box::new(MarkdownPassthroughFixture {
            name: "exec_command",
            schema: Some(crate::tool::text_output_schema()),
            data: None,
        })],
        crate::types::ToolLogLevel::Info,
    );
    let mut config = handler.config.as_ref().clone();
    config.markdown_chat.enabled = true;
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("oversize-inbox").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.ensure().await.unwrap();
    let before: Value =
        serde_json::from_slice(&std::fs::read(chat.path().with_file_name("cursor.json")).unwrap())
            .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(chat.path())
        .unwrap()
        .write_all(&vec![b'x'; crate::markdown_chat::MAX_UNREAD_BYTES + 1])
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
    let mut request = CallToolRequestParams::new("exec_command");
    request.meta =
        Some(serde_json::from_value(json!({"openai/session":"oversize-inbox"})).unwrap());
    let blocked = client.call_tool(request).await.unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert!(
        !blocked
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
    );
    assert!(
        blocked.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Required next tool: chat_read")
    );
    assert!(blocked.structured_content.as_ref().is_none_or(|value| {
        value
            .get(crate::markdown_chat::USER_MESSAGE_FIELD)
            .is_none()
    }));
    let after: Value =
        serde_json::from_slice(&std::fs::read(chat.path().with_file_name("cursor.json")).unwrap())
            .unwrap();
    assert_eq!(after["offset"], before["offset"]);
    assert_eq!(after["deliveredThrough"], before["deliveredThrough"]);
    assert!(chat.read(false).await.err().unwrap().contains("16 MiB"));
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn inline_pending_chat_read_cancelled_and_empty_receipts_do_not_claim_delivery() {
    let root = tempfile::tempdir().unwrap();
    let mut handler = handler_with_tools(root.path(), vec![], crate::types::ToolLogLevel::Info);
    let mut config = handler.config.as_ref().clone();
    config.markdown_chat.enabled = true;
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("cancel-inline-inbox").unwrap();
    bind_fixture_workspace(&handler, Some(&identity)).await;
    let chat = handler
        .markdown_chat
        .chat(&config, Some(&identity), &handler.session)
        .unwrap();
    chat.append_user(
        "cancelled-owner".into(),
        "Do not consume a cancelled read".into(),
    )
    .await
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let context = pending_chat_context(&handler, &identity, cancellation);
    assert!(
        read_pending_chat_inline(&config, &handler.session, &context)
            .await
            .is_none()
    );
    assert!(
        !chat
            .user_messages_read(&["cancelled-owner".into()])
            .await
            .unwrap()
    );
    let context = pending_chat_context(&handler, &identity, CancellationToken::new());
    let receipt = read_pending_chat_inline(&config, &handler.session, &context)
        .await
        .unwrap();
    assert!(
        receipt.is_error,
        "this is a rejected work operation, never work success"
    );
    assert!(
        receipt
            .new_chat_message_from_user
            .unwrap()
            .contains("Do not consume a cancelled read")
    );
    assert!(
        chat.user_messages_read(&["cancelled-owner".into()])
            .await
            .unwrap()
    );
    assert!(
        read_pending_chat_inline(&config, &handler.session, &context)
            .await
            .is_none(),
        "empty/racing reads retain the caller's explicit-read fallback"
    );
}

#[tokio::test]
async fn inline_pending_chat_read_keeps_ticket_rejection_and_successor_handoff_guards() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(TicketProbe {
            name: "exec_command",
            calls: calls.clone(),
            private: false,
        }),
        Box::new(crate::tools::markdown_chat::ChatTool::Write),
    ];
    let mut handler = handler_with_tools(root.path(), tools, crate::types::ToolLogLevel::Info);
    let mut config = handler.config.as_ref().clone();
    config.markdown_chat.enabled = true;
    config.experimental.agent_tickets = true;
    handler.config = Arc::new(config.clone());
    bind_fixture_workspace(&handler, None).await;
    let chat = handler
        .markdown_chat
        .chat(&config, None, &handler.session)
        .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
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
    let first = client
        .call_tool(ticket_request("exec_command", None, json!({})))
        .await
        .unwrap();
    let ticket = next_ticket(&first);
    chat.append_user(
        "ticketed-owner".into(),
        "Only the winning ticket may read this direction".into(),
    )
    .await
    .unwrap();
    let rejected = client
        .call_tool(ticket_request("exec_command", None, json!({})))
        .await
        .unwrap();
    assert_ticket_rejected(&rejected);
    assert!(
        !serde_json::to_string(&rejected)
            .unwrap()
            .contains("Only the winning ticket")
    );
    assert!(
        !chat
            .user_messages_read(&["ticketed-owner".into()])
            .await
            .unwrap()
    );
    let blocked = client
        .call_tool(ticket_request("exec_command", Some(&ticket), json!({})))
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the winning work operation is blocked, never replayed"
    );
    assert!(
        chat.user_messages_read(&["ticketed-owner".into()])
            .await
            .unwrap()
    );
    let next = next_ticket(&blocked);
    assert_ne!(next, ticket);
    let acknowledgment = client
        .call_tool(ticket_request(
            "chat_write",
            Some(&next),
            json!({"message":"Acknowledged the direction."}),
        ))
        .await
        .unwrap();
    assert_ne!(acknowledgment.is_error, Some(true));
    let next = next_ticket(&acknowledgment);
    let worked = client
        .call_tool(ticket_request("exec_command", Some(&next), json!({})))
        .await
        .unwrap();
    assert_ne!(worked.is_error, Some(true));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    client.cancel().await.unwrap();
    server.await.unwrap();
}
