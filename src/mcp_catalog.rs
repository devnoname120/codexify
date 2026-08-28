//! Progressive disclosure for transitively discovered MCP tools.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, Implementation, ServerPeerInfo, Tool as McpTool};
use rmcp::service::Peer;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::bridge::forward_tool_call;
use crate::exec_sessions::SessionState;
use crate::tool::{Tool, ToolBehavior, ToolCallIdentity, ToolRequestContext};
use crate::types::{AppConfig, McpServerProvenance, ToolResult};

pub(crate) const MCP_LIST_SOURCES: &str = "mcp_list_sources";
pub(crate) const MCP_SEARCH_TOOLS: &str = "mcp_search_tools";
pub(crate) const MCP_GET_TOOL: &str = "mcp_get_tool";
pub(crate) const MCP_CALL_TOOL: &str = "mcp_call_tool";

const DEFAULT_SOURCE_LIMIT: usize = 50;
const MAX_SOURCE_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;
const MAX_DESCRIPTION_CHARS: usize = 500;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

pub(crate) struct CatalogSourceInput {
    pub raw_name: String,
    pub provenance: McpServerProvenance,
    pub transport: String,
    pub peer_info: Option<Arc<ServerPeerInfo>>,
    pub tools: Vec<McpTool>,
    pub peer: Peer<RoleClient>,
    pub tool_timeout: Option<Duration>,
}

struct CatalogTool {
    id: String,
    raw: McpTool,
}

struct CatalogSource {
    id: String,
    raw_name: String,
    provenance: McpServerProvenance,
    transport: String,
    implementation: Option<Implementation>,
    instructions: Option<String>,
    tools: Vec<CatalogTool>,
    tool_by_id: HashMap<String, usize>,
    peer: Peer<RoleClient>,
    tool_timeout: Option<Duration>,
}

struct SearchDocument {
    source_index: usize,
    tool_index: usize,
    term_frequency: HashMap<String, f64>,
    length: f64,
    normalized_name: String,
    normalized_title: String,
}

struct SearchIndex {
    documents: Vec<SearchDocument>,
    document_frequency: HashMap<String, usize>,
    average_length: f64,
}

struct Catalog {
    sources: Vec<CatalogSource>,
    source_by_id: HashMap<String, usize>,
    index: SearchIndex,
}

impl Catalog {
    fn new(mut inputs: Vec<CatalogSourceInput>) -> Self {
        inputs.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
        let mut used_sources = HashSet::new();
        let mut sources = Vec::with_capacity(inputs.len());

        for input in inputs {
            let source_id = unique_identifier(
                model_identifier(&input.raw_name, "source"),
                &mut used_sources,
            );
            let mut used_tools = HashSet::new();
            let mut raw_tools = input.tools;
            raw_tools.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.title.cmp(&right.title))
                    .then_with(|| left.description.cmp(&right.description))
            });
            let mut tools = Vec::with_capacity(raw_tools.len());
            let mut tool_by_id = HashMap::new();

            for raw in raw_tools {
                let id =
                    unique_identifier(model_identifier(raw.name.as_ref(), "tool"), &mut used_tools);
                tool_by_id.insert(id.clone(), tools.len());
                tools.push(CatalogTool { id, raw });
            }

            let (implementation, instructions) = input
                .peer_info
                .as_deref()
                .map(|info| (info.server_info.clone(), info.instructions.clone()))
                .unwrap_or((None, None));
            sources.push(CatalogSource {
                id: source_id,
                raw_name: input.raw_name,
                provenance: input.provenance,
                transport: input.transport,
                implementation,
                instructions,
                tools,
                tool_by_id,
                peer: input.peer,
                tool_timeout: input.tool_timeout,
            });
        }

        let source_by_id = sources
            .iter()
            .enumerate()
            .map(|(index, source)| (source.id.clone(), index))
            .collect();
        let index = SearchIndex::build(&sources);
        Self {
            sources,
            source_by_id,
            index,
        }
    }

    fn source(&self, id: &str) -> Result<&CatalogSource, String> {
        self.source_by_id
            .get(id)
            .and_then(|index| self.sources.get(*index))
            .ok_or_else(|| {
                format!("Unknown MCP source id '{id}'. Call `{MCP_LIST_SOURCES}` first.")
            })
    }

    fn tool<'a>(&'a self, source: &'a CatalogSource, id: &str) -> Result<&'a CatalogTool, String> {
        source
            .tool_by_id
            .get(id)
            .and_then(|index| source.tools.get(*index))
            .ok_or_else(|| {
                format!(
                    "Unknown tool id '{id}' for MCP source '{}'. Search with `{MCP_SEARCH_TOOLS}` first.",
                    source.id
                )
            })
    }

    fn manifest_description(&self) -> String {
        let mut entries = self
            .sources
            .iter()
            .take(20)
            .map(|source| {
                if source.id == source.raw_name {
                    format!("`{}` ({} tools)", source.id, source.tools.len())
                } else {
                    format!(
                        "`{}` / `{}` ({} tools)",
                        source.id,
                        source.raw_name,
                        source.tools.len()
                    )
                }
            })
            .collect::<Vec<_>>();
        if self.sources.len() > entries.len() {
            entries.push(format!("{} more", self.sources.len() - entries.len()));
        }
        if entries.is_empty() {
            "No catalog-mode sources are connected.".to_string()
        } else {
            format!("Connected private sources: {}.", entries.join(", "))
        }
    }

    fn source_summary(source: &CatalogSource) -> Value {
        let mut value = Map::new();
        value.insert("id".into(), Value::String(source.id.clone()));
        value.insert("name".into(), Value::String(source.raw_name.clone()));
        value.insert(
            "provenance".into(),
            Value::String(source.provenance.as_str().to_string()),
        );
        value.insert("transport".into(), Value::String(source.transport.clone()));
        value.insert("exposure".into(), Value::String("catalog".into()));
        value.insert("toolCount".into(), json!(source.tools.len()));
        if let Some(implementation) = &source.implementation
            && let Ok(serialized) = serde_json::to_value(implementation)
        {
            value.insert("implementation".into(), serialized);
        }
        if let Some(instructions) = &source.instructions {
            value.insert("instructions".into(), Value::String(instructions.clone()));
        }
        Value::Object(value)
    }

    fn tool_summary(source: &CatalogSource, tool: &CatalogTool, score: f64) -> Value {
        let description = tool
            .raw
            .description
            .as_deref()
            .map(|description| truncate_chars(description, MAX_DESCRIPTION_CHARS));
        json!({
            "sourceId": source.id,
            "sourceName": source.raw_name,
            "toolId": tool.id,
            "toolName": tool.raw.name,
            "title": tool.raw.title,
            "description": description,
            "score": rounded_score(score),
        })
    }

    fn tool_metadata(source: &CatalogSource, tool: &CatalogTool) -> Value {
        let mut serialized = serde_json::to_value(&tool.raw)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        serialized.insert("id".into(), Value::String(tool.id.clone()));
        json!({
            "source": Self::source_summary(source),
            "tool": serialized,
        })
    }

    fn list_sources(&self, query: Option<&str>, limit: usize) -> Value {
        let query_terms = query.map(tokenize).unwrap_or_default();
        let mut sources = self
            .sources
            .iter()
            .filter(|source| source_matches(source, &query_terms))
            .map(Self::source_summary)
            .collect::<Vec<_>>();
        let total = sources.len();
        sources.truncate(limit);
        json!({
            "sources": sources,
            "total": total,
        })
    }

    fn search(&self, query: &str, source_id: Option<&str>, limit: usize) -> Result<Value, String> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Err("`query` must contain at least one letter or number".to_string());
        }
        let source_filter = source_id
            .map(|id| {
                self.source_by_id.get(id).copied().ok_or_else(|| {
                    format!("Unknown MCP source id '{id}'. Call `{MCP_LIST_SOURCES}` first.")
                })
            })
            .transpose()?;

        let mut scored = self.index.search(&query_terms, query, source_filter);
        let total = scored.len();
        scored.truncate(limit);
        let matches = scored
            .into_iter()
            .map(|(document_index, score)| {
                let document = &self.index.documents[document_index];
                let source = &self.sources[document.source_index];
                let tool = &source.tools[document.tool_index];
                Self::tool_summary(source, tool, score)
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "query": query,
            "source": source_id,
            "matches": matches,
            "total": total,
        }))
    }
}

impl SearchIndex {
    fn build(sources: &[CatalogSource]) -> Self {
        let mut documents = Vec::new();
        let mut document_frequency = HashMap::new();
        let mut total_length = 0.0;

        for (source_index, source) in sources.iter().enumerate() {
            for (tool_index, tool) in source.tools.iter().enumerate() {
                let document = SearchDocument::new(source_index, tool_index, source, tool);
                for term in document.term_frequency.keys() {
                    *document_frequency.entry(term.clone()).or_insert(0) += 1;
                }
                total_length += document.length;
                documents.push(document);
            }
        }

        let average_length = if documents.is_empty() {
            1.0
        } else {
            total_length / documents.len() as f64
        };
        Self {
            documents,
            document_frequency,
            average_length,
        }
    }

    fn search(
        &self,
        query_terms: &[String],
        raw_query: &str,
        source_filter: Option<usize>,
    ) -> Vec<(usize, f64)> {
        let mut query_frequency = HashMap::new();
        for term in query_terms {
            *query_frequency.entry(term.as_str()).or_insert(0_u32) += 1;
        }
        let normalized_query = tokenize(raw_query).join(" ");
        let document_count = self.documents.len() as f64;
        let mut scored = Vec::new();

        for (document_index, document) in self.documents.iter().enumerate() {
            if source_filter.is_some_and(|source| source != document.source_index) {
                continue;
            }
            let mut score = 0.0;
            for (term, query_count) in &query_frequency {
                let Some(term_frequency) = document.term_frequency.get(*term) else {
                    continue;
                };
                let document_frequency = *self.document_frequency.get(*term).unwrap_or(&0) as f64;
                let inverse_document_frequency = (1.0
                    + (document_count - document_frequency + 0.5) / (document_frequency + 0.5))
                    .ln();
                let length_normalization =
                    BM25_K1 * (1.0 - BM25_B + BM25_B * document.length / self.average_length);
                let term_score = inverse_document_frequency * (term_frequency * (BM25_K1 + 1.0))
                    / (term_frequency + length_normalization);
                score += term_score * (1.0 + (*query_count as f64).ln());
            }

            if document.normalized_name == normalized_query {
                score += 12.0;
            } else if !normalized_query.is_empty()
                && document.normalized_name.contains(&normalized_query)
            {
                score += 4.0;
            }
            if !normalized_query.is_empty() && document.normalized_title == normalized_query {
                score += 6.0;
            }
            if score > 0.0 {
                scored.push((document_index, score));
            }
        }

        scored.sort_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left_index.cmp(right_index))
        });
        scored
    }
}

impl SearchDocument {
    fn new(
        source_index: usize,
        tool_index: usize,
        source: &CatalogSource,
        tool: &CatalogTool,
    ) -> Self {
        let mut term_frequency = HashMap::new();
        add_text(&mut term_frequency, &source.id, 1.5);
        add_text(&mut term_frequency, &source.raw_name, 1.5);
        add_text(&mut term_frequency, source.provenance.as_str(), 0.5);
        add_text(&mut term_frequency, &source.transport, 0.5);
        if let Some(implementation) = &source.implementation {
            add_text(&mut term_frequency, &implementation.name, 0.75);
            add_optional_text(&mut term_frequency, implementation.title.as_deref(), 0.75);
            add_optional_text(
                &mut term_frequency,
                implementation.description.as_deref(),
                0.5,
            );
        }
        add_optional_text(&mut term_frequency, source.instructions.as_deref(), 0.4);
        add_text(&mut term_frequency, &tool.id, 5.0);
        add_text(&mut term_frequency, tool.raw.name.as_ref(), 5.0);
        add_optional_text(&mut term_frequency, tool.raw.title.as_deref(), 4.0);
        add_optional_text(&mut term_frequency, tool.raw.description.as_deref(), 2.0);
        collect_schema_terms(
            &Value::Object((*tool.raw.input_schema).clone()),
            &mut term_frequency,
        );
        if let Some(output_schema) = &tool.raw.output_schema {
            collect_schema_terms(
                &Value::Object((**output_schema).clone()),
                &mut term_frequency,
            );
        }
        let length = term_frequency.values().sum::<f64>().max(1.0);
        Self {
            source_index,
            tool_index,
            term_frequency,
            length,
            normalized_name: tokenize(tool.raw.name.as_ref()).join(" "),
            normalized_title: tool
                .raw
                .title
                .as_deref()
                .map(tokenize)
                .unwrap_or_default()
                .join(" "),
        }
    }
}

fn add_text(terms: &mut HashMap<String, f64>, text: &str, weight: f64) {
    for token in tokenize(text) {
        *terms.entry(token).or_insert(0.0) += weight;
    }
}

fn add_optional_text(terms: &mut HashMap<String, f64>, text: Option<&str>, weight: f64) {
    if let Some(text) = text {
        add_text(terms, text, weight);
    }
}

fn collect_schema_terms(value: &Value, terms: &mut HashMap<String, f64>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "properties" | "$defs" | "definitions" => {
                        if let Value::Object(properties) = value {
                            for (name, property) in properties {
                                add_text(terms, name, 2.0);
                                collect_schema_terms(property, terms);
                            }
                        }
                    }
                    "title" => add_optional_text(terms, value.as_str(), 1.5),
                    "description" => add_optional_text(terms, value.as_str(), 1.0),
                    "required" => {
                        if let Value::Array(required) = value {
                            for name in required.iter().filter_map(Value::as_str) {
                                add_text(terms, name, 1.25);
                            }
                        }
                    }
                    "enum" => {
                        if let Value::Array(values) = value {
                            for item in values.iter().filter_map(Value::as_str) {
                                add_text(terms, item, 0.4);
                            }
                        }
                    }
                    _ => collect_schema_terms(value, terms),
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_schema_terms(value, terms);
            }
        }
        _ => {}
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let characters = text.chars().collect::<Vec<_>>();

    for (index, character) in characters.iter().copied().enumerate() {
        if character.is_alphanumeric() {
            let previous = index
                .checked_sub(1)
                .and_then(|previous| characters.get(previous))
                .copied();
            let next = characters.get(index + 1).copied();
            let starts_word = character.is_uppercase()
                && !current.is_empty()
                && (previous
                    .is_some_and(|previous| previous.is_lowercase() || previous.is_ascii_digit())
                    || (previous.is_some_and(char::is_uppercase)
                        && next.is_some_and(char::is_lowercase)));
            if starts_word {
                tokens.push(current.to_lowercase());
                current.clear();
            }
            current.extend(character.to_lowercase());
        } else {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn model_identifier(raw: &str, fallback: &str) -> String {
    let mut identifier = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if identifier.is_empty() || identifier.chars().all(|character| character == '_') {
        identifier = fallback.to_string();
    }
    identifier.truncate(64);
    identifier
}

fn unique_identifier(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    for suffix_number in 2..10_000 {
        let suffix = format!("_{suffix_number}");
        let mut candidate = base.clone();
        candidate.truncate(64usize.saturating_sub(suffix.len()));
        candidate.push_str(&suffix);
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("identifier space exhausted")
}

fn source_matches(source: &CatalogSource, query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return true;
    }
    let mut searchable = format!(
        "{} {} {} {}",
        source.id,
        source.raw_name,
        source.provenance.as_str(),
        source.transport
    );
    if let Some(implementation) = &source.implementation {
        searchable.push(' ');
        searchable.push_str(&implementation.name);
        if let Some(title) = &implementation.title {
            searchable.push(' ');
            searchable.push_str(title);
        }
        if let Some(description) = &implementation.description {
            searchable.push(' ');
            searchable.push_str(description);
        }
    }
    if let Some(instructions) = &source.instructions {
        searchable.push(' ');
        searchable.push_str(instructions);
    }
    let tokens = tokenize(&searchable).into_iter().collect::<HashSet<_>>();
    query_terms.iter().all(|term| tokens.contains(term))
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut characters = text.chars();
    let truncated = characters.by_ref().take(limit).collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn rounded_score(score: f64) -> f64 {
    (score * 1_000.0).round() / 1_000.0
}

fn parse_limit(args: &Value, default: usize, maximum: usize) -> Result<usize, String> {
    let Some(value) = args.get("limit") else {
        return Ok(default);
    };
    let Some(value) = value.as_u64() else {
        return Err("`limit` must be an integer".to_string());
    };
    let value = usize::try_from(value).map_err(|_| "`limit` is too large".to_string())?;
    if value == 0 || value > maximum {
        return Err(format!("`limit` must be between 1 and {maximum}"));
    }
    Ok(value)
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

fn structured_json(value: Value) -> ToolResult {
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    ToolResult::text(rendered).with_structured(value)
}

fn catalog_read_behavior() -> ToolBehavior {
    ToolBehavior::new(
        true,
        false,
        true,
        false,
        "Reads the in-process catalog of already connected MCP sources without invoking an upstream tool.",
    )
}

fn dispatcher_behavior() -> ToolBehavior {
    ToolBehavior::new(
        false,
        true,
        false,
        true,
        "Can dispatch to any cataloged upstream tool, including destructive and open-world operations.",
    )
}

fn icon_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "src": { "type": "string" },
            "mimeType": { "type": "string" },
            "sizes": { "type": "array", "items": { "type": "string" } },
            "theme": { "type": "string", "enum": ["light", "dark"] }
        },
        "required": ["src"],
        "additionalProperties": false
    })
}

fn implementation_schema() -> Value {
    let icon = icon_schema();
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "title": { "type": "string" },
            "version": { "type": "string" },
            "description": { "type": "string" },
            "icons": { "type": "array", "items": icon },
            "websiteUrl": { "type": "string" }
        },
        "required": ["name", "version"],
        "additionalProperties": false
    })
}

fn annotations_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "readOnlyHint": { "type": "boolean" },
            "destructiveHint": { "type": "boolean" },
            "idempotentHint": { "type": "boolean" },
            "openWorldHint": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn source_summary_schema() -> Value {
    let implementation = implementation_schema();
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "name": { "type": "string" },
            "provenance": { "type": "string", "enum": ["explicit", "codex-config", "codex-cli"] },
            "transport": { "type": "string" },
            "exposure": { "type": "string", "const": "catalog" },
            "toolCount": { "type": "integer", "minimum": 0 },
            "implementation": implementation,
            "instructions": { "type": "string" }
        },
        "required": ["id", "name", "provenance", "transport", "exposure", "toolCount"],
        "additionalProperties": false
    })
}

fn tool_summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sourceId": { "type": "string" },
            "sourceName": { "type": "string" },
            "toolId": { "type": "string" },
            "toolName": { "type": "string" },
            "title": { "type": ["string", "null"] },
            "description": { "type": ["string", "null"] },
            "score": { "type": "number", "minimum": 0 }
        },
        "required": [
            "sourceId",
            "sourceName",
            "toolId",
            "toolName",
            "title",
            "description",
            "score"
        ],
        "additionalProperties": false
    })
}

fn upstream_tool_schema() -> Value {
    let annotations = annotations_schema();
    let icon = icon_schema();
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "name": { "type": "string" },
            "title": { "type": "string" },
            "description": { "type": "string" },
            "inputSchema": { "type": "object" },
            "outputSchema": { "type": "object" },
            "annotations": annotations,
            "icons": { "type": "array", "items": icon },
            "_meta": { "type": "object" }
        },
        "required": ["id", "name", "inputSchema"],
        "additionalProperties": false
    })
}

struct McpListSourcesTool {
    catalog: Arc<Catalog>,
    description: String,
}

#[async_trait]
impl Tool for McpListSourcesTool {
    fn name(&self) -> &'static str {
        MCP_LIST_SOURCES
    }

    fn title(&self) -> String {
        "List transitive MCP sources".to_string()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn behavior(&self) -> ToolBehavior {
        catalog_read_behavior()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional full-text filter over source id, raw name, implementation metadata, instructions, provenance, and transport."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SOURCE_LIMIT,
                    "description": format!("Maximum sources to return. Defaults to {DEFAULT_SOURCE_LIMIT}.")
                }
            },
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let source = source_summary_schema();
        Some(json!({
            "type": "object",
            "properties": {
                "sources": { "type": "array", "items": source },
                "total": { "type": "integer", "minimum": 0 }
            },
            "required": ["sources", "total"],
            "additionalProperties": false
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let query = match args.get("query") {
            None | Some(Value::Null) => None,
            Some(Value::String(query)) => Some(query.as_str()),
            Some(_) => return ToolResult::error("`query` must be a string"),
        };
        let limit = match parse_limit(&args, DEFAULT_SOURCE_LIMIT, MAX_SOURCE_LIMIT) {
            Ok(limit) => limit,
            Err(error) => return ToolResult::error(error),
        };
        structured_json(self.catalog.list_sources(query, limit))
    }
}

struct McpSearchToolsTool {
    catalog: Arc<Catalog>,
    description: String,
}

#[async_trait]
impl Tool for McpSearchToolsTool {
    fn name(&self) -> &'static str {
        MCP_SEARCH_TOOLS
    }

    fn title(&self) -> String {
        "Search transitive MCP tools".to_string()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn behavior(&self) -> ToolBehavior {
        catalog_read_behavior()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": "Ranked full-text query. The BM25 index covers source metadata, tool id/name/title/description, and recursively useful input/output schema property names and descriptions."
                },
                "source": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": format!("Optional model-visible source id returned by `{MCP_LIST_SOURCES}`.")
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_LIMIT,
                    "description": format!("Maximum matches to return. Defaults to {DEFAULT_SEARCH_LIMIT}.")
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let tool = tool_summary_schema();
        Some(json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "source": { "type": ["string", "null"] },
                "matches": { "type": "array", "items": tool },
                "total": { "type": "integer", "minimum": 0 }
            },
            "required": ["query", "source", "matches", "total"],
            "additionalProperties": false
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let query = match required_string(&args, "query") {
            Ok(query) => query,
            Err(error) => return ToolResult::error(error),
        };
        let source = match args.get("source") {
            None | Some(Value::Null) => None,
            Some(Value::String(source)) if !source.trim().is_empty() => Some(source.as_str()),
            Some(_) => return ToolResult::error("`source` must be a non-empty string"),
        };
        let limit = match parse_limit(&args, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT) {
            Ok(limit) => limit,
            Err(error) => return ToolResult::error(error),
        };
        match self.catalog.search(query, source, limit) {
            Ok(value) => structured_json(value),
            Err(error) => ToolResult::error(error),
        }
    }
}

struct McpGetToolTool {
    catalog: Arc<Catalog>,
}

#[async_trait]
impl Tool for McpGetToolTool {
    fn name(&self) -> &'static str {
        MCP_GET_TOOL
    }

    fn title(&self) -> String {
        "Get a transitive MCP tool definition".to_string()
    }

    fn description(&self) -> String {
        format!(
            "Retrieve the exact upstream metadata and input/output schemas for one result from `{MCP_SEARCH_TOOLS}`. The response keeps model-visible `id` values separate from raw upstream `name` values and includes upstream title, annotations, icons, and `_meta` when supplied."
        )
    }

    fn behavior(&self) -> ToolBehavior {
        catalog_read_behavior()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": format!("Model-visible source id returned by `{MCP_LIST_SOURCES}` or `{MCP_SEARCH_TOOLS}`.")
                },
                "tool": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": format!("Model-visible tool id returned by `{MCP_SEARCH_TOOLS}`.")
                }
            },
            "required": ["source", "tool"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        let source = source_summary_schema();
        let tool = upstream_tool_schema();
        Some(json!({
            "type": "object",
            "properties": {
                "source": source,
                "tool": tool
            },
            "required": ["source", "tool"],
            "additionalProperties": false
        }))
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        let source_id = match required_string(&args, "source") {
            Ok(source) => source,
            Err(error) => return ToolResult::error(error),
        };
        let tool_id = match required_string(&args, "tool") {
            Ok(tool) => tool,
            Err(error) => return ToolResult::error(error),
        };
        let source = match self.catalog.source(source_id) {
            Ok(source) => source,
            Err(error) => return ToolResult::error(error),
        };
        let tool = match self.catalog.tool(source, tool_id) {
            Ok(tool) => tool,
            Err(error) => return ToolResult::error(error),
        };
        structured_json(Catalog::tool_metadata(source, tool))
    }
}

struct McpCallToolTool {
    catalog: Arc<Catalog>,
}

impl McpCallToolTool {
    async fn run(&self, args: Value, cancellation: Option<&CancellationToken>) -> ToolResult {
        let source_id = match required_string(&args, "source") {
            Ok(source) => source,
            Err(error) => return ToolResult::error(error),
        };
        let tool_id = match required_string(&args, "tool") {
            Ok(tool) => tool,
            Err(error) => return ToolResult::error(error),
        };
        let source = match self.catalog.source(source_id) {
            Ok(source) => source,
            Err(error) => return ToolResult::error(error),
        };
        let tool = match self.catalog.tool(source, tool_id) {
            Ok(tool) => tool,
            Err(error) => return ToolResult::error(error),
        };

        let mut params = CallToolRequestParams::new(tool.raw.name.to_string());
        match args.get("arguments") {
            None | Some(Value::Null) => {}
            Some(Value::Object(arguments)) => {
                if !arguments.is_empty() {
                    params = params.with_arguments(arguments.clone());
                }
            }
            Some(_) => {
                return ToolResult::error(
                    "`arguments` must be a JSON object mapping the selected tool's parameter names to values",
                );
            }
        }

        forward_tool_call(
            &source.peer,
            params,
            &source.raw_name,
            tool.raw.name.as_ref(),
            source.tool_timeout,
            cancellation,
        )
        .await
    }
}

#[async_trait]
impl Tool for McpCallToolTool {
    fn name(&self) -> &'static str {
        MCP_CALL_TOOL
    }

    fn title(&self) -> String {
        "Call a transitive MCP tool".to_string()
    }

    fn description(&self) -> String {
        format!(
            "Invoke a catalog-mode upstream MCP tool selected by the model-visible source/tool ids returned by `{MCP_SEARCH_TOOLS}`. Call `{MCP_GET_TOOL}` first when exact arguments or upstream annotations matter. Results preserve upstream text, images, structured content, result metadata, and `isError`; configured timeouts and downstream cancellation are forwarded. Because this generic dispatcher can select tools with different side effects, its host-facing annotations are necessarily conservative and cannot reproduce per-tool approval semantics."
        )
    }

    fn behavior(&self) -> ToolBehavior {
        dispatcher_behavior()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": format!("Model-visible source id returned by `{MCP_LIST_SOURCES}` or `{MCP_SEARCH_TOOLS}`.")
                },
                "tool": {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "\\S",
                    "description": format!("Model-visible tool id returned by `{MCP_SEARCH_TOOLS}`.")
                },
                "arguments": {
                    "type": "object",
                    "description": format!("Arguments matching the selected tool's input schema from `{MCP_GET_TOOL}`.")
                }
            },
            "required": ["source", "tool"],
            "additionalProperties": false
        })
    }

    fn call_identity(&self, args: &Value) -> ToolCallIdentity {
        let source = args
            .get("source")
            .and_then(Value::as_str)
            .and_then(|source_id| self.catalog.source(source_id).ok());
        match source {
            Some(source) => {
                let tool = args
                    .get("tool")
                    .and_then(Value::as_str)
                    .and_then(|tool_id| self.catalog.tool(source, tool_id).ok())
                    .map(|tool| tool.raw.name.to_string());
                ToolCallIdentity::mcp(self.name(), source.raw_name.clone(), tool)
            }
            None => ToolCallIdentity::native(self.name()),
        }
    }

    fn fills_structured_content(&self) -> bool {
        false
    }

    fn requires_project_root(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _config: &AppConfig, _session: &SessionState) -> ToolResult {
        self.run(args, None).await
    }

    async fn call_with_context(
        &self,
        args: Value,
        _config: &AppConfig,
        _session: &SessionState,
        context: &ToolRequestContext,
    ) -> ToolResult {
        self.run(args, Some(&context.cancellation)).await
    }
}

pub(crate) fn build_catalog_tools(inputs: Vec<CatalogSourceInput>) -> Vec<Box<dyn Tool>> {
    if inputs.is_empty() {
        return Vec::new();
    }
    let catalog = Arc::new(Catalog::new(inputs));
    let manifest = catalog.manifest_description();
    vec![
        Box::new(McpListSourcesTool {
            catalog: catalog.clone(),
            description: format!(
                "List or filter upstream MCP systems whose full tool catalog is kept private from downstream `tools/list`. {manifest} Use the returned model-visible source ids with `{MCP_SEARCH_TOOLS}`; raw configured names and implementation metadata remain available in the result."
            ),
        }),
        Box::new(McpSearchToolsTool {
            catalog: catalog.clone(),
            description: format!(
                "Search private transitive MCP tools with a local BM25 index instead of loading every upstream schema into the connector manifest. {manifest} Results contain stable model-visible source/tool ids plus raw upstream names; retrieve an exact definition with `{MCP_GET_TOOL}` before calling `{MCP_CALL_TOOL}` when needed."
            ),
        }),
        Box::new(McpGetToolTool {
            catalog: catalog.clone(),
        }),
        Box::new(McpCallToolTool { catalog }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Icon, ToolAnnotations};

    #[test]
    fn tokenizer_splits_names_for_ranked_search() {
        assert_eq!(
            tokenize("renameFunction_by-address"),
            ["rename", "function", "by", "address"]
        );
        assert_eq!(tokenize("readIDBMetadata"), ["read", "idb", "metadata"]);
    }

    #[test]
    fn identifiers_keep_raw_name_collisions_distinct() {
        let mut used = HashSet::new();
        let first = unique_identifier(model_identifier("a-b", "tool"), &mut used);
        let second = unique_identifier(model_identifier("a b", "tool"), &mut used);
        assert_eq!(first, "a_b");
        assert_eq!(second, "a_b_2");
    }

    #[test]
    fn schema_terms_include_recursive_property_names_and_descriptions() {
        let mut terms = HashMap::new();
        collect_schema_terms(
            &json!({
                "type": "object",
                "properties": {
                    "function_address": {
                        "type": "string",
                        "description": "Virtual address to decompile"
                    }
                }
            }),
            &mut terms,
        );
        assert!(terms.contains_key("function"));
        assert!(terms.contains_key("address"));
        assert!(terms.contains_key("decompile"));
    }

    #[test]
    fn catalog_record_schemas_accept_current_rmcp_serialization() {
        let mut implementation = Implementation::new("fixture", "1.0.0");
        implementation.title = Some("Fixture server".to_string());
        implementation.description = Some("Test implementation".to_string());
        implementation.icons = Some(vec![
            Icon::new("https://example.invalid/server.svg")
                .with_mime_type("image/svg+xml")
                .with_sizes(vec!["any".to_string()])
                .with_theme(rmcp::model::IconTheme::Dark),
        ]);
        implementation.website_url = Some("https://example.invalid".to_string());
        let source = json!({
            "id": "fixture",
            "name": "fixture",
            "provenance": "explicit",
            "transport": "streamable-http",
            "exposure": "catalog",
            "toolCount": 1,
            "implementation": implementation,
            "instructions": "Use for fixture operations."
        });
        assert!(
            jsonschema::draft202012::options()
                .build(&source_summary_schema())
                .unwrap()
                .is_valid(&source)
        );

        let raw = McpTool::new(
            "fixture_tool",
            "Fixture tool",
            json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "additionalProperties": false
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .with_title("Fixture tool")
        .with_annotations(
            ToolAnnotations::with_title("Fixture action")
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        )
        .with_icons(vec![Icon::new("https://example.invalid/tool.png")]);
        let mut tool = serde_json::to_value(raw)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        tool.insert("id".to_string(), Value::String("fixture_tool".to_string()));
        assert!(
            jsonschema::draft202012::options()
                .build(&upstream_tool_schema())
                .unwrap()
                .is_valid(&Value::Object(tool))
        );
    }
}
