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
        crate::tool::ToolBehavior::new(true, false, true, false, "Test-only passthrough")
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
    let mut config = crate::config::default_config(root.path().into());
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
    let mut config = crate::config::default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.multi_project = true;
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
        assert_eq!(
            serde_json::to_value(advertised.output_schema).unwrap(),
            json!(tool.output_schema())
        );
    }
}

#[tokio::test]
async fn markdown_chat_dispatch_delivers_full_messages_after_budgeting_and_errors() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("example.txt"), "source\n").unwrap();
    let mut config = crate::config::default_config(root.path().into());
    config.markdown_chat.enabled = true;
    config.memory.dir = Some(root.path().join("metadata").display().to_string());
    config.output.max_tool_output_tokens = Some(100);
    let mut tools = crate::registry::load_tools_for_config(&config);
    tools.push(Box::new(InvalidStructuredOutputTool));
    let mut handler = handler_with_tools(root.path(), tools, crate::types::ToolLogLevel::Info);
    handler.config = Arc::new(config.clone());
    let identity = ConversationIdentity::from_openai_session("chat-dispatch").unwrap();
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
    let mut config = crate::config::default_config(root.path().into());
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
